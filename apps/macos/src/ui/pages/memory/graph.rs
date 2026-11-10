//! The Memory page's graph: memory pages as nodes and `[[links]]` as edges, moved by a live
//! force simulation in the manner of d3-force: nodes keep velocity under friction while a
//! cooling `alpha` scales every force, so the graph springs into place, and a dragged page
//! reheats it and pulls its neighbours after it.

use std::sync::Arc;

use crate::numeric::ToFloat;
use mobius_gateway::wire::{MemoryLink, MemoryRecord};

/// The simulated world is this wide and one unit tall; the canvas maps it onto its bounds.
pub(super) const WIDTH: f32 = 1.8;
const ALPHA_MIN: f32 = 0.001;
const ALPHA_DECAY: f32 = 0.0228;
/// Lower than d3's 0.4 so motion overshoots a little before it settles.
const VELOCITY_DECAY: f32 = 0.25;
/// The temperature kept while a page is dragged.
const DRAG_ALPHA: f32 = 0.3;
const LINK_DISTANCE: f32 = 0.24;
const CHARGE: f32 = -0.0035;
const CENTER: f32 = 0.05;
const MARGIN: f32 = 0.06;

/// One page in the graph.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Node {
    /// Index into the listing.
    pub(super) record: usize,
    pub(super) id: Arc<str>,
    pub(super) x: f32,
    pub(super) y: f32,
    vx: f32,
    vy: f32,
    /// Held under the pointer while dragged.
    pinned: bool,
    degree: f32,
    pub(super) backlinks: usize,
    /// Links nowhere and nothing links to it.
    pub(super) orphan: bool,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Graph {
    pub(super) nodes: Vec<Node>,
    /// Pairs of node indices.
    pub(super) edges: Vec<(usize, usize)>,
    alpha: f32,
    alpha_target: f32,
    /// The node under the pointer, and whether it moved since it was grabbed.
    pub(super) dragging: Option<usize>,
    pub(super) moved: bool,
}

/// Follows the gateway's resolved target; the client does not choose a scope.
pub(super) fn resolve(
    records: &[MemoryRecord],
    from: &MemoryRecord,
    link: &MemoryLink,
) -> Option<usize> {
    let target = link.target.as_ref()?;
    records
        .iter()
        .position(|record| record.harness == from.harness && &record.id == target)
}

/// Every page that links to `records[target]`.
pub(super) fn backlinks(records: &[MemoryRecord], target: usize) -> Vec<usize> {
    records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            record
                .links
                .iter()
                .any(|link| resolve(records, record, link) == Some(target))
        })
        .map(|(index, _)| index)
        .collect()
}

/// How many pages neither link to a page nor are linked from one, without a layout.
pub(super) fn unlinked(records: &[MemoryRecord]) -> usize {
    let mut linked = vec![false; records.len()];
    for (from, record) in records.iter().enumerate() {
        for link in &record.links {
            if let Some(to) = resolve(records, record, link).filter(|to| *to != from) {
                for index in [from, to] {
                    if let Some(flag) = linked.get_mut(index) {
                        *flag = true;
                    }
                }
            }
        }
    }
    linked.iter().filter(|linked| !**linked).count()
}

impl Graph {
    /// The graph for `records`, starting where `previous` left pages that are still listed,
    /// or on a sunflower spiral, so a new listing springs out from the middle.
    pub(super) fn new(records: &[MemoryRecord], previous: Option<&Self>) -> Self {
        let mut edges = Vec::new();
        for (from, record) in records.iter().enumerate() {
            for link in &record.links {
                if let Some(to) = resolve(records, record, link)
                    && to != from
                    && !edges.contains(&(from, to))
                {
                    edges.push((from, to));
                }
            }
        }
        let total = records.len().max(1).to_f32();
        let mut kept = 0;
        let nodes = records
            .iter()
            .enumerate()
            .map(|(record, page)| {
                let outgoing = edges.iter().filter(|edge| edge.0 == record).count();
                let backlinks = edges.iter().filter(|edge| edge.1 == record).count();
                let radius = ((record.to_f32() + 0.5) / total).sqrt() * 0.3;
                let angle = record.to_f32() * 2.399_963;
                let (x, y) = previous
                    .and_then(|graph| graph.nodes.iter().find(|node| node.id == page.id))
                    .map_or_else(
                        || {
                            (
                                WIDTH / 2.0 + radius * angle.cos(),
                                0.5 + radius * angle.sin(),
                            )
                        },
                        |node| {
                            kept += 1;
                            (node.x, node.y)
                        },
                    );
                Node {
                    record,
                    id: Arc::clone(&page.id),
                    x,
                    y,
                    vx: 0.0,
                    vy: 0.0,
                    pinned: false,
                    degree: (outgoing + backlinks).max(1).to_f32(),
                    backlinks,
                    orphan: outgoing == 0 && backlinks == 0,
                }
            })
            .collect();
        Self {
            nodes,
            edges,
            // A listing that mostly kept its pages only nudges into place.
            alpha: if kept * 2 > records.len() {
                DRAG_ALPHA
            } else {
                1.0
            },
            alpha_target: 0.0,
            dragging: None,
            moved: false,
        }
    }

    /// Whether another tick would still move anything.
    pub(super) fn hot(&self) -> bool {
        self.alpha > ALPHA_MIN || self.dragging.is_some()
    }

    /// Runs the simulation to rest, for reduced motion and captures.
    pub(super) fn settle(&mut self) {
        self.alpha_target = 0.0;
        while self.alpha > ALPHA_MIN {
            self.tick();
        }
    }

    /// One step: cool, apply repulsion, link springs and centering, then integrate.
    pub(super) fn tick(&mut self) {
        self.alpha += (self.alpha_target - self.alpha) * ALPHA_DECAY;
        let alpha = self.alpha;
        // ponytail: O(n²) repulsion per tick; fine for a few hundred pages, Barnes–Hut beyond.
        let pushes: Vec<(f32, f32)> = self
            .nodes
            .iter()
            .map(|a| {
                self.nodes.iter().filter(|b| b.record != a.record).fold(
                    (0.0, 0.0),
                    |(px, py), b| {
                        let (dx, dy) = (a.x - b.x, a.y - b.y);
                        let squared = (dx * dx + dy * dy).max(1e-4);
                        let strength = -CHARGE * alpha / squared;
                        (px + dx * strength, py + dy * strength)
                    },
                )
            })
            .collect();
        for (node, (px, py)) in self.nodes.iter_mut().zip(pushes) {
            node.vx += px + (WIDTH / 2.0 - node.x) * CENTER * alpha;
            node.vy += py + (0.5 - node.y) * CENTER * alpha;
        }
        for &(a, b) in &self.edges {
            let (Some(from), Some(to)) = (self.nodes.get(a), self.nodes.get(b)) else {
                continue;
            };
            let (degree_a, degree_b) = (from.degree, to.degree);
            let (dx, dy) = (
                to.x + to.vx - from.x - from.vx,
                to.y + to.vy - from.y - from.vy,
            );
            let distance = (dx * dx + dy * dy).sqrt().max(1e-4);
            let pull = (distance - LINK_DISTANCE) / distance * alpha / degree_a.min(degree_b);
            let bias = degree_a / (degree_a + degree_b);
            if let Some(to) = self.nodes.get_mut(b) {
                to.vx -= dx * pull * bias;
                to.vy -= dy * pull * bias;
            }
            if let Some(from) = self.nodes.get_mut(a) {
                from.vx += dx * pull * (1.0 - bias);
                from.vy += dy * pull * (1.0 - bias);
            }
        }
        for node in &mut self.nodes {
            if node.pinned {
                node.vx = 0.0;
                node.vy = 0.0;
                continue;
            }
            node.vx *= 1.0 - VELOCITY_DECAY;
            node.vy *= 1.0 - VELOCITY_DECAY;
            node.x = (node.x + node.vx).clamp(MARGIN, WIDTH - MARGIN);
            node.y = (node.y + node.vy).clamp(MARGIN, 1.0 - MARGIN);
        }
    }

    /// Holds `node` under the pointer and keeps the graph warm while it moves.
    pub(super) fn grab(&mut self, node: usize) {
        if let Some(held) = self.nodes.get_mut(node) {
            held.pinned = true;
            self.dragging = Some(node);
            self.moved = false;
            self.alpha_target = DRAG_ALPHA;
            self.alpha = self.alpha.max(DRAG_ALPHA);
        }
    }

    /// Moves the held page to `(x, y)` in world units.
    pub(super) fn drag_to(&mut self, x: f32, y: f32) {
        if let Some(held) = self.dragging.and_then(|node| self.nodes.get_mut(node)) {
            self.moved |= (held.x - x).abs() + (held.y - y).abs() > 0.005;
            held.x = x.clamp(MARGIN, WIDTH - MARGIN);
            held.y = y.clamp(MARGIN, 1.0 - MARGIN);
        }
    }

    /// Lets go: the page rejoins the simulation and the graph cools again.
    pub(super) fn release(&mut self) {
        if let Some(held) = self
            .dragging
            .take()
            .and_then(|node| self.nodes.get_mut(node))
        {
            held.pinned = false;
        }
        self.alpha_target = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mobius_gateway::wire::{HarnessKind, WorkspaceInfo};

    fn page(name: &str, project: Option<&str>, links: &[(&str, Option<&str>)]) -> MemoryRecord {
        MemoryRecord {
            id: format!("{}/{name}", project.unwrap_or("global")).into(),
            name: name.into(),
            description: format!("about {name}").into(),
            links: links
                .iter()
                .map(|(name, target)| mobius_gateway::wire::MemoryLink {
                    name: (*name).into(),
                    target: target.map(Into::into),
                })
                .collect(),
            harness: HarnessKind::Mobius,
            project: project.map(|id| WorkspaceInfo {
                id: id.to_owned(),
                path: format!("/work/{id}").into(),
            }),
            bot_id: project.map(|_| "bot".into()),
            global: project.is_none(),
            text: "body".into(),
            updated_at: 1,
            editable: true,
        }
    }

    #[test]
    fn gateway_targets_drive_edges_backlinks_and_layout() {
        let records = [
            page(
                "deploy",
                Some("app"),
                &[("office", Some("app/office")), ("missing", None)],
            ),
            page("office", Some("app"), &[]),
            page("office", None, &[]),
            page(
                "release",
                Some("site"),
                &[("office", Some("global/office"))],
            ),
            page("loner", Some("site"), &[]),
        ];
        let mut graph = Graph::new(&records, None);
        graph.settle();
        assert_eq!(graph.edges, [(0, 1), (3, 2)]);
        assert_eq!(backlinks(&records, 2), [3]);
        assert!(graph.nodes[4].orphan && !graph.nodes[0].orphan);
        assert_eq!(unlinked(&records), 1);
        assert!(
            graph
                .nodes
                .iter()
                .all(|node| (0.0..=WIDTH).contains(&node.x) && (0.0..=1.0).contains(&node.y))
        );
        // A dragged page pulls its linked neighbour after it.
        let target = (WIDTH - 0.1, 0.1);
        let gap = |node: &Node| (node.x - target.0).hypot(node.y - target.1);
        let before = gap(&graph.nodes[1]);
        graph.grab(0);
        graph.drag_to(target.0, target.1);
        for _ in 0..120 {
            graph.tick();
        }
        assert!(gap(&graph.nodes[1]) < before);
        // Reduce Motion must settle even while the pointer still holds a page.
        graph.settle();
        assert!(graph.alpha <= ALPHA_MIN);
        assert_eq!(graph.dragging, Some(0));
        assert_eq!((graph.nodes[0].x, graph.nodes[0].y), target);
        graph.release();
        assert!(graph.dragging.is_none());
        assert!(!graph.hot());
    }
}
