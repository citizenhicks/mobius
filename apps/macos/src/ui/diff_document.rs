//! Patch text for gpui-component's strict diff parser. Git diffs and turn changes are
//! rewritten as one unified diff whose hunk counts match the lines that arrived, so a diff
//! cut at the transfer limit still parses.

use crate::gateway::TranscriptEntry;
use gpui_component::diff::DiffFile;
use mobius::protocol::FrontendBlockFormat;
use std::fmt::Write as _;

const TRUNCATED: &str = "[diff truncated]";

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
    pub fn parse(source: &str) -> Self {
        Self::parse_in(source, "")
    }

    /// Parses `source`, naming files inside `root` relative to it.
    pub fn parse_in(source: &str, root: &str) -> Self {
        let mut patch = Patch {
            root: root.trim_end_matches('/'),
            ..Patch::default()
        };
        if let Err(error) = patch.feed(source) {
            tracing::warn!(%error, "the diff text could not be written");
        }
        Self {
            files: DiffFile::parse(&patch.out).unwrap_or_default(),
            truncated: patch.truncated,
        }
    }

    /// A unified diff block's files, parsed once per version of the item.
    pub fn of(entry: &TranscriptEntry) -> Option<&Self> {
        entry
            .derived(|entry| {
                entry
                    .block()
                    .filter(|block| block.format == FrontendBlockFormat::UnifiedDiff)
                    .map(|block| Self::parse(&block.text))
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

fn relative<'p>(root: &str, path: &'p str) -> &'p str {
    (!root.is_empty())
        .then(|| path.strip_prefix(root)?.strip_prefix('/'))
        .flatten()
        .unwrap_or(path)
}

#[derive(Default)]
struct Hunk<'a> {
    old: usize,
    new: usize,
    context: &'a str,
    lines: Vec<&'a str>,
    /// Lines the header still promises on each side; a cut hunk ends early.
    promised: (usize, usize),
}

#[derive(Default)]
struct Patch<'a> {
    root: &'a str,
    out: String,
    hunk: Option<Hunk<'a>>,
    truncated: bool,
}

impl<'a> Patch<'a> {
    fn feed(&mut self, source: &'a str) -> std::fmt::Result {
        for line in source.lines() {
            let line = line.trim_suffix('\r');
            if line == TRUNCATED {
                self.truncated = true;
            } else {
                self.unified_line(line)?;
            }
        }
        self.flush_hunk()
    }

    fn unified_line(&mut self, line: &'a str) -> std::fmt::Result {
        if let Some(hunk) = &mut self.hunk {
            let (old, new) = &mut hunk.promised;
            let body = match line.as_bytes().first() {
                Some(b'\\') => Some(line),
                // Transports may strip the space from an empty context line.
                Some(b' ') | None if *old > 0 && *new > 0 => {
                    (*old, *new) = (*old - 1, *new - 1);
                    Some(if line.is_empty() { " " } else { line })
                }
                Some(b'-') if *old > 0 => {
                    *old -= 1;
                    Some(line)
                }
                Some(b'+') if *new > 0 => {
                    *new -= 1;
                    Some(line)
                }
                _ => None,
            };
            if let Some(body) = body {
                hunk.lines.push(body);
                return Ok(());
            }
        }
        self.flush_hunk()?;
        if line.starts_with("@@") {
            let range = |sign| -> (usize, usize) {
                let mut range = line
                    .split(' ')
                    .find_map(|field| field.strip_prefix(sign))
                    .unwrap_or_default()
                    .split(',')
                    .map(|value| value.parse().unwrap_or(0));
                (range.next().unwrap_or(1), range.next().unwrap_or(1))
            };
            let ((old, old_count), (new, new_count)) = (range('-'), range('+'));
            let context = line.get(2..).and_then(|rest| rest.split_once("@@"));
            self.hunk = Some(Hunk {
                old,
                new,
                context: context.map_or("", |(_, context)| context),
                lines: Vec::new(),
                promised: (old_count, new_count),
            });
            return Ok(());
        }
        for prefix in ["--- ", "+++ ", "rename from ", "rename to "] {
            if let Some(path) = line.strip_prefix(prefix) {
                writeln!(self.out, "{prefix}{}", relative(self.root, path))?;
                return Ok(());
            }
        }
        if !line.is_empty() {
            writeln!(self.out, "{line}")?;
        }
        Ok(())
    }

    /// Writes the hunk with counts taken from its lines.
    fn flush_hunk(&mut self) -> std::fmt::Result {
        let Some(hunk) = self.hunk.take() else {
            return Ok(());
        };
        let count = |sign| {
            hunk.lines
                .iter()
                .filter(|line| line.starts_with([' ', sign]))
                .count()
        };
        let (old, new) = (count('-'), count('+'));
        if old + new == 0 {
            return Ok(());
        }
        let fits = |start: usize, count| start.checked_add(count)?.checked_add(1);
        if fits(hunk.old, old).is_none() || fits(hunk.new, new).is_none() {
            self.truncated = true;
            return Ok(());
        }
        let start = |line: usize, count| if count == 0 { line } else { line.max(1) };
        writeln!(
            self.out,
            "@@ -{},{old} +{},{new} @@{}",
            start(hunk.old, old),
            start(hunk.new, new),
            hunk.context
        )?;
        for line in &hunk.lines {
            writeln!(self.out, "{line}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::DiffDocument;

    #[test]
    fn a_truncated_hunk_keeps_the_lines_that_arrived() {
        let document = DiffDocument::parse(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1,9 +1,9 @@\n-old\n+new\n[diff truncated]\n",
        );
        assert!(document.truncated);
        assert_eq!((document.added(), document.removed()), (1, 1));
    }

    #[test]
    fn overflowing_line_numbers_cannot_crash_the_diff_view() {
        let document = DiffDocument::parse(&format!(
            "--- a/x\n+++ b/x\n@@ -{},2 +1,2 @@\n a\n b\n",
            usize::MAX
        ));
        assert!(document.truncated);
        assert_eq!((document.added(), document.removed()), (0, 0));
    }
}
