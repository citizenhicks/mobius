use crate::gateway::TranscriptEntry;
use gpui_component::diff::DiffFile;
use mobius::protocol::FrontendBlockFormat;
use std::borrow::Cow;

pub struct DiffDocument {
    pub files: Vec<DiffFile>,
    pub truncated: bool,
}

impl std::fmt::Debug for DiffDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiffDocument")
            .field("files", &self.files.len())
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl DiffDocument {
    /// Parses `source`, naming files inside `root` relative to it.
    pub fn parse_in(source: &str, root: &str, truncated: bool) -> Self {
        let root = root.trim_end_matches('/');
        let patch = if root.is_empty() {
            Cow::Borrowed(source)
        } else {
            Cow::Owned(relative_paths(source, root))
        };
        Self {
            files: DiffFile::parse(&patch).unwrap_or_default(),
            truncated,
        }
    }

    /// A unified diff block's files, parsed once per version of the item.
    pub fn of(entry: &TranscriptEntry) -> Option<&Self> {
        entry
            .derived(|entry| {
                entry
                    .block()
                    .filter(|block| block.format == FrontendBlockFormat::UnifiedDiff)
                    .map(|block| Self::parse_in(&block.text, "", false))
            })
            .and_then(Option::as_ref)
    }

    pub fn added(&self) -> usize {
        self.files.iter().map(DiffFile::additions).sum()
    }

    pub fn removed(&self) -> usize {
        self.files.iter().map(DiffFile::deletions).sum()
    }
}

fn relative_paths(source: &str, root: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut remaining = (0_usize, 0_usize);
    for line in source.lines() {
        let body = match line.as_bytes().first() {
            Some(b' ') if remaining.0 > 0 && remaining.1 > 0 => {
                remaining = (remaining.0 - 1, remaining.1 - 1);
                true
            }
            Some(b'-') if remaining.0 > 0 => {
                remaining.0 -= 1;
                true
            }
            Some(b'+') if remaining.1 > 0 => {
                remaining.1 -= 1;
                true
            }
            _ => false,
        };
        if body {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if let Some(header) = line.strip_prefix("@@ -") {
            let count = |range: &str| {
                range
                    .split_once(',')
                    .map_or(Some(1), |(_, count)| count.parse().ok())
                    .unwrap_or(0)
            };
            let mut ranges = header.split_whitespace();
            remaining = (
                count(ranges.next().unwrap_or_default()),
                count(ranges.next().unwrap_or_default()),
            );
        }
        let path = ["--- ", "+++ ", "rename from ", "rename to "]
            .into_iter()
            .find_map(|prefix| Some((prefix, line.strip_prefix(prefix)?)));
        match path {
            Some((prefix, path)) => {
                out.push_str(prefix);
                out.push_str(relative(root, path));
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    out
}

pub(in crate::ui) fn relative<'p>(root: &str, path: &'p str) -> &'p str {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return path;
    }
    path.strip_prefix(root)
        .and_then(|path| path.strip_prefix('/'))
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::DiffDocument;

    #[test]
    fn files_inside_the_chat_folder_are_named_relative_to_it() {
        let document = DiffDocument::parse_in(
            "--- /srv/app/x\n+++ /srv/app/x\n@@ -1 +1 @@\n-old\n+new\n",
            "/srv/app/",
            false,
        );
        let paths: Vec<String> = document
            .files
            .iter()
            .map(|file| file.path().to_string())
            .collect();
        assert_eq!(paths, ["x"]);
    }

    #[test]
    fn header_like_source_lines_keep_their_contents() {
        let source = "--- /srv/app/x\n+++ /srv/app/x\n@@ -1 +1 @@\n--- /srv/app/old\n+++ /srv/app/new\n--- /srv/app/y\n+++ /srv/app/y\n@@ -1 +1 @@\n-before\n+after\n";
        let expected = "--- x\n+++ x\n@@ -1 +1 @@\n--- /srv/app/old\n+++ /srv/app/new\n--- y\n+++ y\n@@ -1 +1 @@\n-before\n+after\n";
        assert_eq!(super::relative_paths(source, "/srv/app"), expected);
        assert_eq!(
            DiffDocument::parse_in(source, "/srv/app", false)
                .files
                .len(),
            2
        );
    }
}
