use diffy::{DiffOptions, Line, Patch};

use super::{MAX_PATCH_MATCH_WORK, MAX_TOOL_UI_BYTES, capped};
use crate::{Error, Result};

pub(super) struct PatchDocument {
    pub(super) path: String,
    changes: Vec<PatchChange>,
}

#[derive(Default)]
struct PatchChange {
    anchor: Option<String>,
    before: String,
    after: String,
    end_of_file: bool,
}

pub(super) fn parse_patch_document(input: &str) -> Result<PatchDocument> {
    let mut lines = input
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line));
    if lines.next() != Some("*** Begin Patch") {
        return Err(malformed_patch_document("missing `*** Begin Patch`"));
    }
    let path = lines
        .next()
        .and_then(|line| line.strip_prefix("*** Update File: "))
        .filter(|path| !path.is_empty())
        .ok_or_else(|| malformed_patch_document("expected one `*** Update File: path`"))?
        .to_string();
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
                change.anchor = Some(context.to_string());
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
    let mut updated = content.to_string();
    let mut cursor = 0;
    let mut match_work = 0;
    let line_ending = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    for change in &document.changes {
        if let Some(anchor) = &change.anchor {
            cursor += find_patch_anchor(&updated[cursor..], anchor).ok_or_else(|| {
                Error::Tool(format!(
                    "Patch rejected: context {:?} was not found after the previous change.",
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
                return Err(Error::Tool(
                    "Patch rejected: the end-of-file change did not match the file.".into(),
                ));
            }
            cursor = start;
        }

        let mut options = DiffOptions::new();
        options.set_context_len(before.lines().count().max(after.lines().count()));
        let patch = options.create_patch(&before, &after);
        let suffix = &updated[cursor..];
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
            if let Err(error) = diffy::apply(suffix, &patch) {
                return Err(unmatched_patch_error(suffix, &patch, &error));
            }
            return Err(Error::Tool("patch rejected: context was not found".into()));
        };
        cursor += match_start;
        if patch.hunks().is_empty() {
            cursor += before.len();
            continue;
        }
        let suffix = &updated[cursor..];
        let patched = diffy::apply(suffix, &patch)
            .map_err(|error| unmatched_patch_error(suffix, &patch, &error))?;
        updated.replace_range(cursor.., &patched);
        cursor += after.len();
    }
    Ok(updated)
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
        if content[offset..].starts_with(fragment) {
            return Some(offset);
        }
        offset += content[offset..].find('\n')? + 1;
    }
}

fn unmatched_patch_error(
    content: &str,
    patch: &Patch<'_, str>,
    error: &diffy::ApplyError,
) -> Error {
    let message = error.to_string();
    let Some(hunk_number) = message
        .strip_prefix("error applying hunk #")
        .and_then(|number| number.parse::<usize>().ok())
        .filter(|number| *number > 0 && *number <= patch.hunks().len())
    else {
        return Error::Tool(format!(
            "Patch rejected: a hunk did not match the file.\nReason: {message}."
        ));
    };
    let rejection = if patch.hunks().len() == 1 {
        "Patch rejected: no hunks matched the file.".into()
    } else {
        format!("Patch rejected: hunk #{hunk_number} did not match the file.")
    };
    let Some(hunk) = patch.hunks().get(hunk_number - 1) else {
        return Error::Tool(format!(
            "Patch rejected: a hunk did not match the file.\nReason: {message}."
        ));
    };
    let Some((heading, context)) = hunk.lines().iter().find_map(|line| match line {
        Line::Context(value) if !value.trim().is_empty() => {
            Some(("Failed hunk starts with context:", *value))
        }
        Line::Delete(value) if !value.trim().is_empty() => {
            Some(("Failed hunk starts with deletion:", *value))
        }
        Line::Insert(_) => None,
        Line::Context(_) | Line::Delete(_) => None,
    }) else {
        return Error::Tool(format!(
            "{rejection}\nThe failed hunk has no usable context lines."
        ));
    };
    let nearest = content
        .split_inclusive('\n')
        .enumerate()
        .filter(|(_, line)| *line == context)
        .map(|(index, _)| index + 1)
        .min_by_key(|line| line.abs_diff(hunk.new_range().start()));
    let location = nearest.map_or_else(
        || "No matching context line was found.".into(),
        |line| format!("The nearest match is at line {line}."),
    );
    let context = capped(context.trim_end_matches(['\r', '\n']), MAX_TOOL_UI_BYTES);
    Error::Tool(format!("{rejection}\n{heading}\n{context:?}\n{location}"))
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
