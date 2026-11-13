use diffy::{DiffOptions, Line, Patch};
use std::fmt::Write as _;

use super::{MAX_PATCH_MATCH_WORK, MAX_TOOL_UI_BYTES, capped};
use crate::{Error, Result};

pub(super) struct PatchDocument {
    pub(super) path: String,
    changes: Vec<PatchChange>,
}

impl PatchDocument {
    /// The envelope as one unified diff. Its changes carry no line numbers, so each hunk
    /// is numbered past the previous one with a line between them.
    pub(super) fn unified_diff(&self) -> std::result::Result<String, std::fmt::Error> {
        let mut diff = format!("--- {0}\n+++ {0}\n", self.path);
        let (mut old, mut new) = (1, 1);
        for change in &self.changes {
            let mut options = DiffOptions::new();
            options.set_context_len(
                change
                    .before
                    .lines()
                    .count()
                    .max(change.after.lines().count()),
            );
            let patch = options.create_patch(&change.before, &change.after);
            for hunk in patch.hunks() {
                let (old_len, new_len) = (hunk.old_range().len(), hunk.new_range().len());
                let anchor = change.anchor.as_deref().unwrap_or_default();
                let separator = if anchor.is_empty() { "" } else { " " };
                writeln!(
                    diff,
                    "@@ -{old},{old_len} +{new},{new_len} @@{separator}{anchor}"
                )?;
                for line in hunk.lines() {
                    let (sign, text) = match line {
                        Line::Context(text) => (' ', *text),
                        Line::Delete(text) => ('-', *text),
                        Line::Insert(text) => ('+', *text),
                    };
                    diff.push(sign);
                    diff.push_str(text);
                    if !text.ends_with('\n') {
                        diff.push('\n');
                    }
                }
                old += old_len + 1;
                new += new_len + 1;
            }
        }
        Ok(diff)
    }
}

#[derive(Default)]
struct PatchChange {
    anchor: Option<String>,
    before: String,
    after: String,
    end_of_file: bool,
}

pub(super) fn parse_patch_document(input: &str) -> Result<PatchDocument> {
    let mut lines = input.lines().map(|line| line.trim_suffix('\r'));
    if lines.next() != Some("*** Begin Patch") {
        return Err(malformed_patch_document("missing `*** Begin Patch`"));
    }
    let path = lines
        .next()
        .and_then(|line| line.strip_prefix("*** Update File: "))
        .filter(|path| !path.is_empty())
        .ok_or_else(|| malformed_patch_document("expected one `*** Update File: path`"))?
        .to_owned();
    let mut changes = Vec::new();
    let mut change = PatchChange::default();

    while let Some(line) = lines.next() {
        if line == "*** End Patch" {
            push_patch_change(&mut changes, &mut change);
            if lines.next().is_some() {
                return Err(malformed_patch_document(
                    "`*** End Patch` must be the final line",
                ));
            }
            if changes.is_empty() {
                return Err(malformed_patch_document("the patch contains no changes"));
            }
            return Ok(PatchDocument { path, changes });
        }
        if change.end_of_file {
            if line.is_empty() {
                continue;
            }
            return Err(malformed_patch_document(
                "`*** End of File` must end its change",
            ));
        }
        if line == "@@" || line.starts_with("@@ ") {
            push_patch_change(&mut changes, &mut change);
            if let Some(context) = line.strip_prefix("@@ ").filter(|value| !value.is_empty()) {
                change.anchor = Some(context.to_owned());
            }
            continue;
        }
        if line == "*** End of File" {
            if change.before.is_empty() && change.after.is_empty() {
                return Err(malformed_patch_document("`*** End of File` has no change"));
            }
            change.end_of_file = true;
            continue;
        }
        if line.starts_with("*** ") {
            return Err(malformed_patch_document(
                "only one existing-file `*** Update File` operation is supported",
            ));
        }
        if let Some(value) = line.strip_prefix('+') {
            push_patch_line(&mut change.after, value);
        } else if let Some(value) = line.strip_prefix('-') {
            push_patch_line(&mut change.before, value);
        } else if let Some(value) = line.strip_prefix(' ') {
            push_patch_line(&mut change.before, value);
            push_patch_line(&mut change.after, value);
        } else if line.is_empty() {
            push_patch_line(&mut change.before, "");
            push_patch_line(&mut change.after, "");
        } else {
            return Err(malformed_patch_document(
                "change lines must begin with ` `, `+`, or `-`",
            ));
        }
    }
    Err(malformed_patch_document("missing `*** End Patch`"))
}

fn push_patch_change(changes: &mut Vec<PatchChange>, change: &mut PatchChange) {
    if !change.before.is_empty() || !change.after.is_empty() {
        changes.push(std::mem::take(change));
    }
}

fn push_patch_line(target: &mut String, line: &str) {
    target.push_str(line);
    target.push('\n');
}

fn malformed_patch_document(reason: &str) -> Error {
    Error::Tool(format!(
        "Patch rejected: malformed apply_patch input.\nReason: {reason}."
    ))
}

pub(super) fn apply_patch_document(content: &str, document: &PatchDocument) -> Result<String> {
    let mut updated = content.to_owned();
    let mut cursor = 0;
    let mut match_work = 0;
    let mut added_lines = 0_usize;
    let mut removed_lines = 0_usize;
    let line_ending = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let changes = document.changes.len();
    for (index, change) in document.changes.iter().enumerate() {
        let number = index + 1;
        if let Some(anchor) = &change.anchor {
            cursor +=
                find_patch_anchor(patch_tail(&updated, cursor)?, anchor).ok_or_else(|| {
                    Error::Tool(format!(
                        "Patch rejected: change {number} of {changes}: context {:?} was not found after the previous change. No changes were written.",
                        capped(anchor, MAX_TOOL_UI_BYTES)
                    ))
                })?;
        }
        let mut before = change.before.replace('\n', line_ending);
        let mut after = change.after.replace('\n', line_ending);

        if before.is_empty() {
            if !updated.is_empty() && !updated.ends_with('\n') {
                updated.push_str(line_ending);
            }
            updated.push_str(&after);
            cursor = updated.len();
            continue;
        }
        if change.end_of_file && !updated.ends_with('\n') {
            if before.ends_with(line_ending) {
                before.truncate(before.len() - line_ending.len());
            }
            if after.ends_with(line_ending) {
                after.truncate(after.len() - line_ending.len());
            }
        }
        if change.end_of_file {
            let start = updated.len().saturating_sub(before.len());
            if start < cursor || !updated.ends_with(&before) {
                return Err(Error::Tool(format!(
                    "Patch rejected: change {number} of {changes}: the end-of-file change did not match the file. No changes were written."
                )));
            }
            cursor = start;
        }

        let mut options = DiffOptions::new();
        options.set_context_len(before.lines().count().max(after.lines().count()));
        let patch = options.create_patch(&before, &after);
        let suffix = patch_tail(&updated, cursor)?;
        if patch.hunks().is_empty() {
            charge_patch_work(
                &mut match_work,
                suffix
                    .lines()
                    .count()
                    .saturating_mul(before.len().saturating_add(before.lines().count())),
            )?;
        } else {
            validate_patch_complexity(suffix, &patch, &mut match_work)?;
        }
        let Some(match_start) = find_patch_fragment(suffix, &before) else {
            return Err(unmatched_change_error(
                &updated,
                cursor,
                &before,
                number,
                changes,
                added_lines,
                removed_lines,
            ));
        };
        cursor += match_start;
        if patch.hunks().is_empty() {
            cursor += before.len();
            continue;
        }
        let suffix = patch_tail(&updated, cursor)?;
        let patched = diffy::apply(suffix, &patch)
            .map_err(|error| Error::Tool(format!("Patch rejected: {error}.")))?;
        updated.replace_range(cursor.., &patched);
        cursor += after.len();
        added_lines += after.matches('\n').count();
        removed_lines += before.matches('\n').count();
    }
    Ok(updated)
}

fn patch_tail(content: &str, cursor: usize) -> Result<&str> {
    content
        .get(cursor..)
        .ok_or_else(|| Error::Tool(format!("patch cursor {cursor} is outside the file")))
}

fn find_patch_anchor(content: &str, anchor: &str) -> Option<usize> {
    let mut offset = 0;
    let mut trimmed_match = None;
    for line in content.split_inclusive('\n') {
        let value = line.trim_end_matches(['\r', '\n']);
        if value == anchor {
            return Some(offset + line.len());
        }
        if trimmed_match.is_none() && value.trim() == anchor.trim() {
            trimmed_match = Some(offset + line.len());
        }
        offset += line.len();
    }
    trimmed_match
}

fn find_patch_fragment(content: &str, fragment: &str) -> Option<usize> {
    let mut offset = 0;
    loop {
        let rest = content.get(offset..)?;
        if rest.starts_with(fragment) {
            return Some(offset);
        }
        offset += rest.find('\n')? + 1;
    }
}

/// Names the failed change and the first line where its closest positional match diverges,
/// numbered from the start of the file. The caller has already charged this scan's work.
fn unmatched_change_error(
    content: &str,
    cursor: usize,
    before: &str,
    number: usize,
    changes: usize,
    added_lines: usize,
    removed_lines: usize,
) -> Error {
    let base = content
        .get(..cursor)
        .unwrap_or_default()
        .matches('\n')
        .count()
        .saturating_add(removed_lines)
        .saturating_sub(added_lines);
    let file = content
        .get(cursor..)
        .unwrap_or_default()
        .lines()
        .collect::<Vec<_>>();
    let expected = before.lines().collect::<Vec<_>>();
    // ponytail: positional line overlap picks the window; reordered or edited lines still score.
    let window = |start: usize| file.get(start..).unwrap_or_default().iter().zip(&expected);
    let start = (0..file.len())
        .max_by_key(|&start| {
            let score = window(start).filter(|(line, want)| line == want).count();
            (score, std::cmp::Reverse(start))
        })
        .unwrap_or_default();
    let matched = window(start)
        .take_while(|(line, want)| line == want)
        .count();
    let head = format!(
        "Patch rejected: change {number} of {changes} did not match the file. No changes were written; earlier changes must be resent."
    );
    let show = |line: &str| capped(line, MAX_TOOL_UI_BYTES);
    if window(start).all(|(line, want)| line != want) {
        return Error::Tool(format!(
            "{head}\nNo similar section was found for {:?}.",
            show(expected.first().copied().unwrap_or_default())
        ));
    }
    let Some(want) = expected.get(matched) else {
        return Error::Tool(format!(
            "{head}\nNo matching section was found for {:?}.",
            show(expected.first().copied().unwrap_or_default())
        ));
    };
    let line = base + start + matched + 1;
    let found = file.get(start + matched).map_or_else(
        || "end of file".to_owned(),
        |found| format!("{:?}", show(found)),
    );
    Error::Tool(format!(
        "{head}\nClosest match: first {matched} line(s) match from line {}; line {line} should be {:?} but the file has {found}.",
        base + start + 1,
        show(want)
    ))
}

pub(super) fn validate_patch_complexity(
    content: &str,
    patch: &Patch<'_, str>,
    total_work: &mut usize,
) -> Result<()> {
    let image_lines = content.lines().count().saturating_add(
        patch
            .hunks()
            .iter()
            .map(|hunk| hunk.new_range().len())
            .sum::<usize>(),
    );
    let work = patch.hunks().iter().fold(0_usize, |total, hunk| {
        let mut preimage_lines = 0_usize;
        let mut preimage_bytes = 0_usize;
        for line in hunk.lines() {
            if let Line::Context(value) | Line::Delete(value) = line {
                preimage_lines = preimage_lines.saturating_add(1);
                preimage_bytes = preimage_bytes.saturating_add(value.len());
            }
        }
        let hunk_work = if preimage_lines == 0 {
            hunk.lines().len()
        } else {
            image_lines.saturating_mul(preimage_bytes.saturating_add(hunk.lines().len()))
        };
        total.saturating_add(hunk_work)
    });
    charge_patch_work(total_work, work)
}

fn charge_patch_work(total_work: &mut usize, work: usize) -> Result<()> {
    *total_work = total_work.saturating_add(work);
    if *total_work > MAX_PATCH_MATCH_WORK {
        return Err(Error::Tool("patch is too expensive to match safely".into()));
    }
    Ok(())
}
