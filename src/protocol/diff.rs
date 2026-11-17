//! Line counts of the Git-style unified diffs that [`FrontendBlockFormat::UnifiedDiff`]
//! blocks carry.
//!
//! [`FrontendBlockFormat::UnifiedDiff`]: super::FrontendBlockFormat::UnifiedDiff

use std::iter;

use diffy::Hunk;
use diffy::Line;
use diffy::Patch;

/// Lines a unified diff adds and removes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct UnifiedDiffCounts {
    /// Added lines.
    pub additions: u64,
    /// Removed lines.
    pub deletions: u64,
}

impl UnifiedDiffCounts {
    /// Counts the lines `diff` adds and removes from each file's parsed hunks, so the next
    /// file's `---`/`+++` headers are never counted as changes. Returns `None` when a file
    /// does not parse, as when the diff is incomplete.
    #[must_use]
    pub fn from_diff(diff: &str) -> Option<Self> {
        let mut counts = Self::default();
        for file in unified_diff_files(diff) {
            let patch = Patch::from_str(file).ok()?;
            let file_counts = Self::from(&patch);
            counts.additions += file_counts.additions;
            counts.deletions += file_counts.deletions;
        }
        Some(counts)
    }
}

impl From<&Patch<'_, str>> for UnifiedDiffCounts {
    fn from(patch: &Patch<'_, str>) -> Self {
        let mut counts = Self::default();
        for line in patch.hunks().iter().flat_map(Hunk::lines) {
            match line {
                Line::Insert(_) => counts.additions += 1,
                Line::Delete(_) => counts.deletions += 1,
                Line::Context(_) => {}
            }
        }
        counts
    }
}

/// Splits a Git diff into its files, each starting at a `diff --git ` line; a diff without
/// one is a single file.
pub fn unified_diff_files(diff: &str) -> impl Iterator<Item = &str> {
    let mut start = 0;
    diff.match_indices("\ndiff --git ")
        .map(|(index, _)| index + 1)
        .chain(iter::once(diff.len()))
        .filter_map(move |end| {
            let file = diff.get(start..end);
            start = end;
            file
        })
        .filter(|file| !file.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_two_file_patch_counts_only_hunk_lines() {
        let diff = "diff --git a/one b/one\n--- a/one\n+++ b/one\n@@ -1 +1 @@\n-a\n+b\n\
                    diff --git a/two b/two\n--- a/two\n+++ b/two\n@@ -1 +1 @@\n-c\n+d\n";
        assert_eq!(
            UnifiedDiffCounts::from_diff(diff),
            Some(UnifiedDiffCounts {
                additions: 2,
                deletions: 2,
            })
        );
    }
}
