//! The workspace folder tree: the gateway's flat path list, nested on demand.

use gpui_component::tree::TreeItem;
use std::collections::BTreeSet;
use std::ops::Range;

/// Sorted workspace paths; every folder is the contiguous run sharing its prefix, so
/// a folder's children are only materialized when it is expanded.
pub(super) struct FileTree {
    paths: Vec<String>,
}

struct Child {
    name: String,
    path: String,
    folder: Option<Range<usize>>,
}

impl FileTree {
    pub fn new(paths: impl IntoIterator<Item = String>) -> Self {
        let mut paths: Vec<String> = paths
            .into_iter()
            .map(|path| path.trim_start_matches("./").to_owned())
            .filter(|path| !path.is_empty() && !path.ends_with('/'))
            .collect();
        paths.sort_unstable();
        paths.dedup();
        Self { paths }
    }

    /// Whether `path` is a listed file (not a folder).
    pub fn contains(&self, path: &str) -> bool {
        self.paths
            .binary_search_by(|listed| listed.as_str().cmp(path))
            .is_ok()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Tree items for the top level and every expanded folder. A collapsed folder holds
    /// one placeholder so the tree draws it as a folder; expanding it rebuilds the items.
    pub fn items(&self, expanded: &BTreeSet<String>) -> Vec<TreeItem> {
        self.items_in("", 0..self.paths.len(), expanded)
    }

    fn items_in(
        &self,
        prefix: &str,
        range: Range<usize>,
        expanded: &BTreeSet<String>,
    ) -> Vec<TreeItem> {
        self.children(prefix, range)
            .into_iter()
            .map(|child| match child.folder {
                None => TreeItem::new(child.path, child.name),
                Some(range) => {
                    let inner = format!("{}/", child.path);
                    let open = expanded.contains(&child.path);
                    let children = if open {
                        self.items_in(&inner, range, expanded)
                    } else {
                        vec![TreeItem::new(inner, "")]
                    };
                    TreeItem::new(child.path, child.name)
                        .expanded(open)
                        .children(children)
                }
            })
            .collect()
    }

    /// Direct children of `prefix` within `range`, folders first in Finder order.
    fn children(&self, prefix: &str, range: Range<usize>) -> Vec<Child> {
        let mut children = Vec::new();
        let mut index = range.start;
        let mut remaining = self.paths.get(range).unwrap_or_default();
        while let Some((first, _)) = remaining.split_first() {
            let rest = first.get(prefix.len()..).unwrap_or_default();
            let taken = if let Some((name, _)) = rest.split_once('/') {
                let path = format!("{prefix}{name}");
                let inner = format!("{path}/");
                let taken = remaining.partition_point(|p| p.starts_with(&inner));
                children.push(Child {
                    name: name.to_owned(),
                    path,
                    folder: Some(index..index + taken),
                });
                taken
            } else {
                children.push(Child {
                    name: rest.to_owned(),
                    path: first.clone(), // rules-allow: the tree item owns its path while the listing keeps it
                    folder: None,
                });
                1
            };
            index += taken;
            remaining = remaining.get(taken..).unwrap_or_default();
        }
        children.sort_by_cached_key(|child| (child.folder.is_none(), child.name.to_lowercase()));
        children
    }
}
