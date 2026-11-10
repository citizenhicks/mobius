//! The Memory page, opened from the gateway's `memory` rail contribution: every harness's
//! memory pages from `list_memories`, as a list grouped like the chat list's projects with
//! Global first, or as a graph of their `[[links]]`. A page opens with its rendered body,
//! links and backlinks; editable pages are edited there or forgotten.

use super::harness::name as harness_name;
use super::*;
use crate::ui::navigation::workspace_name;
use gpui_component::collapsible::Collapsible;
use gpui_component::input::{InputState, TextareaState};
use gpui_component::text::TextView;
use mobius_gateway::wire::{BotShape, MemoryRecord, ServerMessage, WorkspaceInfo};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

mod graph;

use crate::numeric::ToFloat;

/// The rail contribution the gateway's memory middleware puts in the Navigation slot.
pub(in crate::ui) const PAGE: &str = "memory:navigation";

/// Whether `page` is the Memory page.
pub(in crate::ui) fn is_memory_page(page: &PageTab) -> bool {
    matches!(page, PageTab::Contribution(key) if key == PAGE)
}

/// Where a memory is listed.
#[derive(Debug, PartialEq, Eq)]
enum Scope<'a> {
    Global,
    Project(&'a WorkspaceInfo),
    NoProject,
}

impl Scope<'_> {
    fn of(memory: &MemoryRecord) -> Scope<'_> {
        match &memory.project {
            _ if memory.global => Scope::Global,
            Some(project) => Scope::Project(project),
            None => Scope::NoProject,
        }
    }

    /// The disclosure key, unique per group.
    fn key(&self) -> SharedString {
        match self {
            Self::Global => "global".into(),
            Self::Project(project) => format!("project:{}", project.id).into(),
            Self::NoProject => "no-project".into(),
        }
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Global => 0,
            Self::Project(_) => 1,
            Self::NoProject => 2,
        }
    }

    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Project(a), Self::Project(b)) => a.id == b.id,
            _ => self.rank() == other.rank(),
        }
    }
}

/// One group of the page: Global, a project, or No project, newest memory first.
struct MemoryGroup<'a> {
    scope: Scope<'a>,
    memories: Vec<&'a MemoryRecord>,
}

/// Global first, then projects by their newest memory, then No project.
fn groups(memories: &[MemoryRecord]) -> Vec<MemoryGroup<'_>> {
    let mut sorted: Vec<&MemoryRecord> = memories.iter().collect();
    sorted.sort_by_key(|memory| std::cmp::Reverse(memory.updated_at));
    let mut groups: Vec<MemoryGroup<'_>> = Vec::new();
    for memory in sorted {
        let scope = Scope::of(memory);
        match groups.iter_mut().find(|group| group.scope.same(&scope)) {
            Some(group) => group.memories.push(memory),
            None => groups.push(MemoryGroup {
                scope,
                memories: vec![memory],
            }),
        }
    }
    // Stable: projects keep the order of their newest memory.
    groups.sort_by_key(|group| group.scope.rank());
    groups
}

/// A memory's place in its harness's store.
#[derive(PartialEq, Eq)]
struct MemoryKey {
    harness: mobius_gateway::wire::HarnessKind,
    id: Arc<str>,
}

impl MemoryKey {
    fn of(memory: &MemoryRecord) -> Self {
        Self {
            harness: memory.harness,
            id: Arc::clone(&memory.id),
        }
    }

    fn is(&self, memory: &MemoryRecord) -> bool {
        self.harness == memory.harness && self.id == memory.id
    }
}

/// The open page's editor; Save sends both fields.
struct Editor {
    key: Rc<MemoryKey>,
    description: Entity<InputState>,
    body: Entity<TextareaState>,
}

/// What an answered edit or forget confirms.
enum Change {
    Saved,
    Forgotten,
}

/// Whether the page lists pages or draws their graph.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum View {
    #[default]
    List,
    Graph,
}

/// Who a page belongs to in the sidebar's summary: everyone, or one Bot (or native store).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Cluster {
    Global,
    Bot(Option<Arc<str>>),
}

impl Cluster {
    fn of(memory: &MemoryRecord) -> Self {
        if memory.global {
            Self::Global
        } else {
            Self::Bot(memory.bot_id.as_ref().map(Arc::clone))
        }
    }
}

/// Package-owned UI state; new fields belong here rather than on `Workspace`.
#[derive(Default)]
pub(in crate::ui) struct MemoryUi {
    view: View,
    raw: bool,
    /// The sidebar cluster the list and graph are narrowed to.
    cluster: Option<Cluster>,
    /// The page shown in full instead of the list or graph.
    open: Option<Rc<MemoryKey>>,
    editor: Option<Editor>,
    /// Edit and forget requests whose `memories` answer a toast confirms.
    pending: BTreeMap<Arc<str>, Change>,
    /// Folded groups, by disclosure key.
    collapsed: BTreeSet<SharedString>,
    /// The live graph and the listing it simulates.
    graph: RefCell<Option<(u64, Rc<RefCell<graph::Graph>>)>>,
    /// Where the graph canvas last painted, to map the pointer into the simulation.
    graph_bounds: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
}

impl MemoryUi {
    pub(in crate::ui) fn is_open(&self) -> bool {
        self.open.is_some()
    }

    fn saving(&self) -> bool {
        self.pending
            .values()
            .any(|change| matches!(change, Change::Saved))
    }

    /// The graph for `memories`; a changed listing restarts it from where pages were.
    fn graph(&self, memories: &[MemoryRecord]) -> Rc<RefCell<graph::Graph>> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for memory in memories {
            (&memory.id, memory.updated_at, &memory.links).hash(&mut hasher);
        }
        let signature = hasher.finish();
        let mut cached = self.graph.borrow_mut();
        match &*cached {
            Some((known, graph)) if *known == signature => Rc::clone(graph),
            previous => {
                let graph = Rc::new(RefCell::new(graph::Graph::new(
                    memories,
                    previous
                        .as_ref()
                        .map(|(_, graph)| graph.borrow())
                        .as_deref(),
                )));
                *cached = Some((signature, Rc::clone(&graph)));
                graph
            }
        }
    }

    /// Runs `change` on the live graph, if one is shown.
    fn with_graph(&self, change: impl FnOnce(&mut graph::Graph)) {
        if let Some((_, graph)) = &*self.graph.borrow() {
            change(&mut graph.borrow_mut());
        }
    }
}

/// Global pages in the accent colour; each project in its own accent, in first-seen order.
fn scope_color<'a>(memory: &'a MemoryRecord, projects: &mut Vec<&'a str>, global: Hsla) -> Hsla {
    if memory.global {
        return global;
    }
    let project = memory
        .project
        .as_ref()
        .map_or("", |project| project.id.as_str());
    let index = projects
        .iter()
        .position(|known| *known == project)
        .unwrap_or_else(|| {
            projects.push(project);
            projects.len() - 1
        });
    Accent::ALL
        .get((index + 1) % Accent::ALL.len())
        .copied()
        .unwrap_or_default()
        .color()
}

/// The links as thin lines, painted under the pages; keeps frames coming while the graph moves.
fn graph_edges(
    shared: &Rc<RefCell<graph::Graph>>,
    bounds: &Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    edge: Hsla,
) -> impl IntoElement {
    let lines = Rc::clone(shared);
    let bounds_out = Rc::clone(bounds);
    canvas(
        move |bounds, _, _| bounds_out.set(Some(bounds)),
        move |bounds, (), window, cx| {
            let lines = lines.borrow();
            let at = |node: &graph::Node| {
                point(
                    bounds.origin.x + bounds.size.width * (node.x / graph::WIDTH),
                    bounds.origin.y + bounds.size.height * node.y,
                )
            };
            for &(from, to) in &lines.edges {
                let (Some(from), Some(to)) = (lines.nodes.get(from), lines.nodes.get(to)) else {
                    continue;
                };
                let mut path = gpui::PathBuilder::stroke(px(1.));
                path.move_to(at(from));
                path.line_to(at(to));
                if let Ok(path) = path.build() {
                    window.paint_path(path, edge);
                }
            }
            if lines.hot() && !cx.reduce_motion() {
                window.request_animation_frame();
            }
        },
    )
    .absolute()
    .size_full()
}

/// `[[page]]` links as Markdown links the page view intercepts.
fn linked_markdown(body: &str) -> String {
    let mut parts = body.split("[[");
    let mut out = parts.next().unwrap_or_default().to_owned();
    for part in parts {
        match part.split_once("]]") {
            Some((name, tail)) if !name.is_empty() && !name.contains(['[', ']', '\n']) => {
                for piece in ["[", name, "](memory-page:", name, ")", tail] {
                    out.push_str(piece);
                }
            }
            _ => {
                out.push_str("[[");
                out.push_str(part);
            }
        }
    }
    out
}

impl Workspace {
    pub(in crate::ui) fn refresh_memories(&mut self, cx: &mut Context<Self>) {
        self.send(
            ClientMessage::ListMemories {
                request_id: crate::gateway::request_id(),
            },
            cx,
        );
    }

    pub(in crate::ui) fn memory_on_message(
        &mut self,
        message: &ServerMessage,
        cx: &mut Context<Self>,
    ) {
        match message {
            // A memory added from the rail widget's own action changes its contribution.
            ServerMessage::Contributions { .. }
                if self.primary_page().is_some_and(is_memory_page) =>
            {
                self.refresh_memories(cx);
            }
            ServerMessage::Memories { request_id, .. } => {
                let Some(change) = self.memory.pending.remove(request_id) else {
                    return;
                };
                let text = match change {
                    Change::Saved => {
                        self.memory.editor = None;
                        "Memory saved."
                    }
                    Change::Forgotten => {
                        self.memory.open = None;
                        "Memory forgotten."
                    }
                };
                self.announce(Tone::Success, text.to_owned());
            }
            // The rejection itself shows as a toast; the editor stays open to try again.
            ServerMessage::Rejected { request_id, .. } => {
                self.memory.pending.remove(request_id);
            }
            ServerMessage::Ready { .. } => self.memory.pending.clear(),
            _ => {}
        }
    }

    pub(in crate::ui) fn render_memory(&self, cx: &Context<Self>) -> AnyElement {
        let memories = self.client.state.memories.as_deref();
        if let (Some(memories), Some(open)) = (memories, &self.memory.open)
            && let Some((index, memory)) = memories
                .iter()
                .enumerate()
                .find(|(_, memory)| open.is(memory))
        {
            return self.memory_page(memories, index, memory, cx);
        }
        let subtitle = memories.map_or_else(String::new, |memories| {
            counted(memories.len(), "page", "pages")
        });
        let header = PageHeader::new("Memory").subtitle(subtitle);
        let body = match memories {
            _ if !self.client.link.is_ready() && memories.is_none() => {
                EmptyState::new("hi.stickyNote02", "Memory unavailable")
                    .detail("Connect to a gateway first.")
                    .into_any_element()
            }
            None => EmptyState::new("hi.stickyNote02", "Loading memories")
                .loading()
                .into_any_element(),
            Some([]) => EmptyState::new("hi.stickyNote02", "No memories yet")
                .detail(
                    "Bots keep a small wiki of linked pages as they work, and tidy it while a \
                     chat sits idle. Each page belongs to the Bot and project it was written \
                     in, unless it is global.",
                )
                .into_any_element(),
            Some(all) => {
                // ponytail: a narrowed view copies its few hundred records per frame at most.
                let narrowed: Vec<MemoryRecord>;
                let memories = match &self.memory.cluster {
                    None => all,
                    Some(cluster) => {
                        narrowed = all
                            .iter()
                            .filter(|memory| Cluster::of(memory) == *cluster)
                            .cloned() // rules-allow: the narrowed listing owns its copies for layout
                            .collect();
                        &narrowed
                    }
                };
                match self.memory.view {
                    View::List => div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .children(
                            groups(memories)
                                .iter()
                                .map(|group| self.memory_group(group, cx)),
                        )
                        .into_any_element(),
                    View::Graph => self.memory_graph(memories, cx),
                }
            }
        };
        page("memory", header).child(body).into_any_element()
    }

    /// The sidebar while the Memory page is open: the List and Graph picker, then the wiki
    /// at a glance by cluster, Global and each Bot; a cluster narrows the page to its pages.
    pub(in crate::ui) fn memory_sidebar(&self, cx: &Context<Self>) -> AnyElement {
        let picker = div().px(space::S).pb(space::S).child(
            segmented(
                "memory-view",
                ["List", "Graph"],
                Some(usize::from(self.memory.view == View::Graph)),
                cx.listener(|this, index: &usize, _, cx| {
                    this.memory.view = if *index == 1 { View::Graph } else { View::List };
                    this.memory.open = None;
                    this.memory.editor = None;
                    cx.notify();
                }),
            )
            .full_width(),
        );
        let mut list = crate::ui::navigation::sidebar_list().child(picker);
        let Some(memories) = self
            .client
            .state
            .memories
            .as_deref()
            .filter(|all| !all.is_empty())
        else {
            return list.into_any_element();
        };
        let mut clusters: BTreeMap<Cluster, usize> = BTreeMap::new();
        for memory in memories {
            *clusters.entry(Cluster::of(memory)).or_default() += 1;
        }
        list = list.child(sidebar_label("Wiki"));
        for (cluster, count) in clusters {
            let (id, title, leading) = match &cluster {
                Cluster::Global => (
                    "global".to_owned(),
                    "Global".to_owned(),
                    Leading::glyph("hi.globe02"),
                ),
                Cluster::Bot(Some(bot_id)) => match self.client.state.bot(bot_id) {
                    Some(bot) => (
                        bot_id.to_string(),
                        bot.name.to_string(),
                        Leading::Face(Accent::from(bot.tint).color(), bot.shape),
                    ),
                    None => (
                        bot_id.to_string(),
                        "A deleted Bot".to_owned(),
                        Leading::glyph("hi.userQuestion01"),
                    ),
                },
                Cluster::Bot(None) => (
                    "native".to_owned(),
                    "Other harnesses".to_owned(),
                    Leading::glyph("hi.folder"),
                ),
            };
            let selected = self.memory.cluster.as_ref() == Some(&cluster);
            list = list.child(
                SidebarRow::new(SharedString::from(format!("memory-cluster:{id}")), title)
                    .leading(leading)
                    .meta(count.to_string())
                    .selected(selected)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.memory.cluster = (!selected).then(|| cluster.clone()); // rules-allow: the Fn handler keeps its cluster for the next click
                        this.memory.open = None;
                        this.memory.editor = None;
                        cx.notify();
                    })),
            );
        }
        let links: usize = memories.iter().map(|memory| memory.links.len()).sum();
        let unlinked = graph::unlinked(memories);
        list.child(
            div()
                .px(space::S)
                .pt(space::S)
                .typography(Type::Caption)
                .text_color(self.palette.muted)
                .child(format!(
                    "{} · {} · {unlinked} unlinked",
                    counted(memories.len(), "page", "pages"),
                    counted(links, "link", "links"),
                )),
        )
        .into_any_element()
    }

    fn open_memory(&mut self, key: Rc<MemoryKey>, cx: &mut Context<Self>) {
        self.memory.editor = None;
        self.memory.open = Some(key);
        cx.notify();
    }

    /// The group's header row, folding it like a project in the chat list, then its rows.
    fn memory_group(&self, group: &MemoryGroup<'_>, cx: &Context<Self>) -> AnyElement {
        let key = group.scope.key();
        let collapsed = self.memory.collapsed.contains(&key);
        let (title, glyph, tooltip): (SharedString, &str, Option<String>) = match &group.scope {
            Scope::Global => ("Global".into(), "hi.globe02", None),
            Scope::Project(project) => {
                let path = project.path.display().to_string();
                let glyph = if collapsed {
                    "hi.folder"
                } else {
                    "hi.folderOpen"
                };
                (workspace_name(&path).to_owned().into(), glyph, Some(path))
            }
            Scope::NoProject => ("No project".into(), "hi.folder", None),
        };
        let toggle = SharedString::clone(&key);
        let header = SidebarRow::new(
            SharedString::from(format!("memory-group:{key}")),
            SharedString::clone(&title),
        )
        .leading(Leading::glyph(glyph))
        .caret(!collapsed)
        .meta(group.memories.len().to_string())
        .accessibility_label(format!(
            "{title}, {}",
            if collapsed { "collapsed" } else { "expanded" }
        ))
        .on_click(cx.listener(move |this, _, _, cx| {
            let collapsed = &mut this.memory.collapsed;
            if !collapsed.remove(&toggle) {
                collapsed.insert(SharedString::clone(&toggle));
            }
            cx.notify();
        }));
        let header = match tooltip {
            Some(path) => header.tooltip(path),
            None => header,
        };
        let mut rows = gpui_component::v_flex().w_full().min_w_0();
        for memory in &group.memories {
            rows = rows.child(self.memory_row(memory, cx));
        }
        div()
            .w_full()
            .mb(space::S)
            .child(header)
            .child(
                Collapsible::new()
                    .motion_id(SharedString::from(format!("memory-reveal:{key}")))
                    .open(!collapsed)
                    .content(
                        div()
                            .w_full()
                            .child(Group::new().child(rows.into_any_element()))
                            .into_any_element(),
                    ),
            )
            .into_any_element()
    }

    /// Who wrote a page: its Bot's name, a deleted Bot, or a native harness.
    fn memory_owner(&self, memory: &MemoryRecord) -> String {
        match (
            memory
                .bot_id
                .as_deref()
                .and_then(|id| self.client.state.bot(id)),
            &memory.bot_id,
        ) {
            (Some(bot), _) => bot.name.to_string(),
            (None, Some(_)) => "A deleted Bot".to_owned(),
            (None, None) => "Unknown Bot".to_owned(),
        }
    }

    /// The page's name, scope, owner and age; opens the page.
    fn memory_row(&self, memory: &MemoryRecord, cx: &Context<Self>) -> AnyElement {
        let bot = memory
            .bot_id
            .as_deref()
            .and_then(|id| self.client.state.bot(id));
        let subtitle = [
            Self::memory_scope(memory),
            self.memory_owner(memory),
            ago(memory.updated_at),
        ];
        let leading = bot.map_or_else(
            || Leading::LargeFace(Accent::White.color(), BotShape::Circle),
            |bot| Leading::LargeFace(Accent::from(bot.tint).color(), bot.shape),
        );
        let id = format!("{}:{}", harness_name(memory.harness), memory.id);
        let key = Rc::new(MemoryKey::of(memory));
        Row::new(
            SharedString::from(format!("memory:{id}")),
            SharedString::from(&*memory.name),
        )
        .leading(leading)
        .subtitle(subtitle.join(" · "))
        .truncate()
        .disclosure()
        .on_click(cx.listener(move |this, _, _, cx| this.open_memory(Rc::clone(&key), cx)))
        .into_any_element()
    }

    /// Pages as dots sized by their backlinks and coloured by scope, links as thin lines,
    /// moved by the live simulation: drag a page and its neighbours spring after it.
    fn memory_graph(&self, memories: &[MemoryRecord], cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let shared = self.memory.graph(memories);
        {
            let mut graph = shared.borrow_mut();
            if cx.reduce_motion() {
                graph.settle();
            } else if graph.hot() {
                graph.tick();
            }
        }
        let graph = shared.borrow();
        let mut projects: Vec<&str> = Vec::new();
        let edges = graph_edges(&shared, &self.memory.graph_bounds, p.separator);
        let nodes = graph.nodes.iter().enumerate().filter_map(|(index, node)| {
            let memory = memories.get(node.record)?;
            let size = glyph::DOT + px(4. * node.backlinks.to_f32().sqrt());
            let key = Rc::new(MemoryKey::of(memory));
            let tint = scope_color(memory, &mut projects, p.button_accent);
            let node_element = div()
                .id(SharedString::from(format!("memory-node:{}", memory.id)))
                .absolute()
                .left(relative(node.x / graph::WIDTH))
                .top(relative(node.y))
                .ml(-size / 2.)
                .mt(-size / 2.)
                .flex()
                .items_center()
                .gap(space::XS)
                .cursor_pointer()
                .when(node.orphan, |node| node.opacity(alpha::DIM))
                .tooltip(text_tooltip(SharedString::from(&*memory.description)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.memory.with_graph(|graph| graph.grab(index));
                        cx.notify();
                    }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let mut moved = false;
                    this.memory.with_graph(|graph| moved = graph.moved);
                    if !moved {
                        this.open_memory(Rc::clone(&key), cx);
                    }
                }))
                .child(div().size(size).pill_corners().bg(tint))
                .child(
                    div()
                        .typography(Type::Caption)
                        .text_color(p.muted)
                        .whitespace_nowrap()
                        .child(SharedString::from(&*memory.name)),
                );
            Some(node_element)
        });
        let release = |this: &mut Self, cx: &mut Context<Self>| {
            this.memory.with_graph(graph::Graph::release);
            cx.notify();
        };
        div()
            .id("memory-graph")
            .relative()
            .w_full()
            .h(height::GRAPH)
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let Some(bounds) = this.memory.graph_bounds.get() else {
                    return;
                };
                let width = f32::from(bounds.size.width).max(1.);
                let height = f32::from(bounds.size.height).max(1.);
                let x = f32::from(event.position.x - bounds.origin.x) / width * graph::WIDTH;
                let y = f32::from(event.position.y - bounds.origin.y) / height;
                let mut dragging = false;
                this.memory.with_graph(|graph| {
                    dragging = graph.dragging.is_some();
                    graph.drag_to(x, y);
                });
                if dragging {
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| release(this, cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| release(this, cx)),
            )
            .child(edges)
            .children(nodes)
            .into_any_element()
    }

    /// One page in full: description, scope, rendered body, links and backlinks.
    fn memory_page(
        &self,
        memories: &[MemoryRecord],
        index: usize,
        memory: &MemoryRecord,
        cx: &Context<Self>,
    ) -> AnyElement {
        let back = inline_button("memory-back", "All pages", "hi.arrowLeft", cx).on_click(
            cx.listener(|this, _, _, cx| {
                this.memory.open = None;
                this.memory.editor = None;
                cx.notify();
            }),
        );
        let header = PageHeader::new(SharedString::from(&*memory.name))
            .leading(back)
            .subtitle(format!(
                "{} · {} · {}",
                Self::memory_scope(memory),
                self.memory_owner(memory),
                ago(memory.updated_at)
            ));
        let editing = self
            .memory
            .editor
            .as_ref()
            .filter(|editor| editor.key.is(memory));
        let mut body = div().w_full().flex().flex_col().gap(space::L);
        body = match editing {
            Some(editor) => body.child(self.memory_editor(editor)),
            None => body
                .child(
                    segmented(
                        "memory-markdown-mode",
                        ["Rendered", "Markdown"],
                        Some(usize::from(self.memory.raw)),
                        cx.listener(|this, index: &usize, _, cx| {
                            this.memory.raw = *index == 1;
                            cx.notify();
                        }),
                    )
                    .full_width(),
                )
                .child(if self.memory.raw {
                    div()
                        .typography(Type::Mono)
                        .child(SharedString::from(&*memory.text))
                        .into_any_element()
                } else {
                    self.memory_page_body(memory, cx)
                }),
        };
        let linked: Vec<usize> = memory
            .links
            .iter()
            .filter_map(|link| graph::resolve(memories, memory, link))
            .collect();
        let backlinks = graph::backlinks(memories, index);
        if !linked.is_empty() || !backlinks.is_empty() {
            body = body.child(hairline(&self.palette));
        }
        for (title, pages) in [("Links", linked), ("Backlinks", backlinks)] {
            let rows: Vec<AnyElement> = pages
                .into_iter()
                .filter_map(|page| memories.get(page))
                .map(|page| self.memory_row(page, cx))
                .collect();
            if !rows.is_empty() {
                let rows = gpui_component::v_flex().w_full().min_w_0().children(rows);
                body = body
                    .child(Section::new(title).child(Group::new().child(rows.into_any_element())));
            }
        }
        page("memory", header).child(body).into_any_element()
    }

    fn memory_scope(memory: &MemoryRecord) -> String {
        match Scope::of(memory) {
            Scope::Global => "Global".to_owned(),
            Scope::Project(project) => {
                workspace_name(&project.path.display().to_string()).to_owned()
            }
            Scope::NoProject => "No project".to_owned(),
        }
    }

    pub(in crate::ui) fn memory_actions(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let Some(memory) = self
            .memory
            .open
            .as_ref()
            .and_then(|key| {
                self.client
                    .state
                    .memories
                    .as_deref()?
                    .iter()
                    .find(|memory| key.is(memory))
            })
            .filter(|memory| memory.editable)
        else {
            return Vec::new();
        };
        let disabled = self.memory.saving() || !self.client.link.is_ready();
        if self.memory.editor.is_some() {
            return vec![
                toolbar_button("memory-cancel", "Cancel editing", "hi.x", cx)
                    .disabled(self.memory.saving())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.memory.editor = None;
                        cx.notify();
                    }))
                    .into_any_element(),
                toolbar_button("memory-save", "Save page", "hi.checkCircle", cx)
                    .disabled(disabled)
                    .on_click(cx.listener(|this, _, _, cx| this.save_memory(cx)))
                    .into_any_element(),
            ];
        }
        let key = Rc::new(MemoryKey::of(memory));
        let id = Arc::clone(&memory.id);
        vec![
            toolbar_button("memory-forget", "Forget page", "hi.trash", cx)
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.confirm_forget(Arc::clone(&id), window, cx);
                }))
                .into_any_element(),
            toolbar_button("memory-edit", "Edit page", "hi.pencilSimple", cx)
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.edit_memory(Rc::clone(&key), window, cx);
                }))
                .into_any_element(),
        ]
    }

    /// The rendered body; a `[[page]]` link opens that page as chats would resolve it.
    fn memory_page_body(&self, memory: &MemoryRecord, cx: &Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        let from = MemoryKey::of(memory);
        TextView::markdown(
            SharedString::from(format!("memory-body:{}", memory.id)),
            SharedString::from(linked_markdown(&memory.text)),
        )
        .selectable(true)
        .style(markdown_style(self.palette.button_accent))
        .on_link_click(move |href, _, _, cx| {
            let Some(name) = href.strip_prefix("memory-page:") else {
                cx.open_url(href);
                return;
            };
            let name = name.to_owned();
            let from = MemoryKey {
                harness: from.harness,
                id: Arc::clone(&from.id),
            };
            if let Err(error) = entity.update(cx, |this, cx| {
                let target = this.client.state.memories.as_deref().and_then(|memories| {
                    let source = memories.iter().find(|memory| from.is(memory))?;
                    let link = source.links.iter().find(|link| *link.name == name)?;
                    let index = graph::resolve(memories, source, link)?;
                    memories.get(index).map(|page| Rc::new(MemoryKey::of(page)))
                });
                match target {
                    Some(key) => this.open_memory(key, cx),
                    None => this.report(format!("No memory page named {name}.")),
                }
            }) {
                tracing::debug!(%error, "the workspace closed before a memory link opened");
            }
        })
        .into_any_element()
    }

    fn memory_editor(&self, editor: &Editor) -> AnyElement {
        let saving = self.memory.saving();
        div()
            .flex()
            .flex_col()
            .gap(space::S)
            .child(
                text_field(&editor.description)
                    .aria_label("Description")
                    .disabled(saving),
            )
            .child(textarea(&editor.body).aria_label("Page").disabled(saving))
            .into_any_element()
    }

    fn edit_memory(&mut self, key: Rc<MemoryKey>, window: &mut Window, cx: &mut Context<Self>) {
        if self.memory.saving() {
            return;
        }
        let Some(memory) = self
            .client
            .state
            .memories
            .iter()
            .flatten()
            .find(|memory| key.is(memory))
        else {
            return;
        };
        let (description, text) = (Arc::clone(&memory.description), Arc::clone(&memory.text));
        let description_input = cx.new(|cx| InputState::new(window, cx));
        description_input.update(cx, |input, cx| input.set_value(&*description, window, cx));
        let body = cx.new(|cx| TextareaState::new(window, cx).auto_grow(6, 24));
        body.update(cx, |input, cx| {
            input.set_value(&*text, window, cx);
            input.focus(window, cx);
        });
        self.memory.editor = Some(Editor {
            key,
            description: description_input,
            body,
        });
        cx.notify();
    }

    /// Sends the editor's fields; an unchanged page just closes the editor.
    fn save_memory(&mut self, cx: &mut Context<Self>) {
        if self.memory.saving() {
            return;
        }
        let Some(editor) = &self.memory.editor else {
            return;
        };
        let description = editor.description.read(cx).value().trim().to_owned();
        let text = editor.body.read(cx).value().trim().to_owned();
        if text.is_empty() || description.is_empty() {
            self.report("A page needs a description and a body. Forget it instead.");
            cx.notify();
            return;
        }
        let unchanged = self
            .client
            .state
            .memories
            .iter()
            .flatten()
            .find(|memory| editor.key.is(memory))
            .is_some_and(|memory| *memory.text == *text && *memory.description == *description);
        if unchanged {
            self.memory.editor = None;
            cx.notify();
            return;
        }
        let request_id = crate::gateway::request_id();
        let id = Arc::clone(&editor.key.id);
        match self.enqueue(ClientMessage::EditMemory {
            request_id: Arc::clone(&request_id),
            id,
            description,
            text,
        }) {
            Ok(()) => {
                self.memory.pending.insert(request_id, Change::Saved);
            }
            Err(error) => self.report(error),
        }
        cx.notify();
    }

    fn confirm_forget(&self, id: Arc<str>, window: &mut Window, cx: &mut Context<Self>) {
        let name = self
            .client
            .state
            .memories
            .iter()
            .flatten()
            .find(|memory| memory.id == id)
            .map_or_else(String::new, |memory| {
                format!("“{}”", single_line(&memory.name))
            });
        self.confirm_destructive(
            "Forget this memory page?",
            format!("{name} No Bot will see it again."),
            "Forget",
            window,
            cx,
            move |this, _, cx| {
                let request_id = crate::gateway::request_id();
                this.memory
                    .pending
                    .insert(Arc::clone(&request_id), Change::Forgotten);
                this.send(
                    ClientMessage::ForgetMemory {
                        request_id,
                        id: Arc::clone(&id),
                    },
                    cx,
                );
            },
        );
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Opens the first editable page, as a click on its row would.
    pub(in crate::ui) fn snapshot_open_first_memory(&mut self, cx: &mut Context<Self>) {
        let first = self
            .client
            .state
            .memories
            .iter()
            .flatten()
            .find(|memory| memory.editable && !memory.links.is_empty())
            .map(|memory| Rc::new(MemoryKey::of(memory)));
        if let Some(key) = first {
            self.open_memory(key, cx);
        }
    }

    /// Opens the first editable page's editor, as a click on Edit would.
    pub(in crate::ui) fn snapshot_edit_first_memory(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.snapshot_open_first_memory(cx);
        if let Some(key) = self.memory.open.as_ref().map(Rc::clone) {
            self.edit_memory(key, window, cx);
        }
    }

    pub(in crate::ui) fn snapshot_memory_graph(&mut self) {
        self.memory.view = View::Graph;
    }

    pub(in crate::ui) fn snapshot_close_memory_editor(&mut self) {
        self.memory.editor = None;
        self.memory.open = None;
        self.memory.view = View::List;
        self.memory.pending.remove("snapshot-memory-save");
    }

    /// Exercises Return and another row's Edit while a save acknowledgement is delayed.
    pub(in crate::ui) fn snapshot_pending_memory_save(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(editor) = self.memory.editor.as_ref() else {
            return false;
        };
        let key = Rc::clone(&editor.key);
        editor.body.update(cx, |input, cx| {
            input.set_value("A fact being saved.", window, cx);
        });
        self.memory
            .pending
            .insert("snapshot-memory-save".into(), Change::Saved);
        self.shell.notices.clear();
        self.save_memory(cx);
        self.edit_memory(
            Rc::new(MemoryKey {
                harness: key.harness,
                id: "another-memory".into(),
            }),
            window,
            cx,
        );
        self.shell.notices.is_empty()
            && self.memory.pending.len() == 1
            && self
                .memory
                .editor
                .as_ref()
                .is_some_and(|editor| editor.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius_gateway::wire::HarnessKind;

    fn memory(id: &str, project: Option<&str>, global: bool, updated_at: i64) -> MemoryRecord {
        MemoryRecord {
            id: id.into(),
            name: id.into(),
            description: "a page".into(),
            links: Vec::new(),
            harness: HarnessKind::Mobius,
            project: project.map(|id| WorkspaceInfo {
                id: id.to_owned(),
                path: format!("/work/{id}").into(),
            }),
            bot_id: Some("bot".into()),
            global,
            text: "a fact".into(),
            updated_at,
            editable: true,
        }
    }

    #[test]
    fn wiki_links_become_page_links_and_other_brackets_stay() {
        assert_eq!(
            linked_markdown("See [[deploy]] and [[ not closed, or [x](y)."),
            "See [deploy](memory-page:deploy) and [[ not closed, or [x](y)."
        );
    }

    #[test]
    fn global_leads_projects_follow_their_newest_memory_and_no_project_ends() {
        let memories = [
            memory("old-one", Some("one"), false, 10),
            memory("loose", None, false, 50),
            memory("two", Some("two"), false, 30),
            memory("shared", None, true, 5),
            memory("new-one", Some("one"), false, 40),
        ];
        let listed: Vec<(SharedString, Vec<&str>)> = groups(&memories)
            .iter()
            .map(|group| {
                (
                    group.scope.key(),
                    group.memories.iter().map(|memory| &*memory.id).collect(),
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("global".into(), vec!["shared"]),
                ("project:one".into(), vec!["new-one", "old-one"]),
                ("project:two".into(), vec!["two"]),
                ("no-project".into(), vec!["loose"]),
            ]
        );
    }
}
