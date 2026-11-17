//! The `apply_patch` envelope: parsing, ordered matching and line-ending-preserving
//! replacement. Ported from the `apply-patch` crate of OpenAI Codex (Apache-2.0, commit
//! a11d2403a5c8f346527379ccd061c2aa95e7edc4) with Mobius's work limit and diagnostics; its
//! filesystem side is Mobius's own, in the sandbox.

use std::cmp::Reverse;
use std::fmt;
use std::fmt::Write as _;

use diffy::DiffOptions;
use diffy::Line;

use super::{MAX_PATCH_MATCH_WORK, MAX_TOOL_UI_BYTES, capped};
use crate::{Error, Result};

const BEGIN_PATCH: &str = "*** Begin Patch";
const END_PATCH: &str = "*** End Patch";
const ADD_FILE: &str = "*** Add File: ";
const DELETE_FILE: &str = "*** Delete File: ";
const UPDATE_FILE: &str = "*** Update File: ";
const MOVE_TO: &str = "*** Move to: ";
const END_OF_FILE: &str = "*** End of File";
const CONTEXT: &str = "@@ ";
const EMPTY_CONTEXT: &str = "@@";
const UNEXPECTED_LINE: &str =
    "Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)";

/// One parsed envelope, its operations in patch order.
pub(super) struct PatchDocument<'patch> {
    pub(super) hunks: Vec<Hunk<'patch>>,
}

/// One file operation, borrowing its paths and lines from the patch text.
pub(super) enum Hunk<'patch> {
    Add {
        path: &'patch str,
        contents: String,
    },
    Delete {
        path: &'patch str,
    },
    Update {
        path: &'patch str,
        move_path: Option<&'patch str>,
        /// In file order: each chunk matches after the previous one.
        chunks: Vec<Chunk<'patch>>,
    },
}

impl<'patch> Hunk<'patch> {
    /// The path the operation reads or creates.
    pub(super) const fn path(&self) -> &'patch str {
        match self {
            Self::Add { path, .. } | Self::Delete { path } | Self::Update { path, .. } => path,
        }
    }
}

impl fmt::Display for Hunk<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add { path, .. } => write!(formatter, "add {path}"),
            Self::Delete { path } => write!(formatter, "delete {path}"),
            Self::Update {
                path,
                move_path: None,
                ..
            } => write!(formatter, "update {path}"),
            Self::Update {
                path,
                move_path: Some(destination),
                ..
            } => write!(formatter, "move {path} to {destination}"),
        }
    }
}

/// One `@@` change of an update.
#[derive(Default)]
pub(super) struct Chunk<'patch> {
    /// A line, usually a definition, that the change must follow.
    change_context: Option<&'patch str>,
    old_lines: Vec<&'patch str>,
    new_lines: Vec<&'patch str>,
    /// Lines parsed as context rather than inferred equal, so they keep their original text.
    context_lines: Vec<ContextLine>,
    /// `old_lines` must end the file.
    is_end_of_file: bool,
}

#[derive(Clone, Copy)]
struct ContextLine {
    old: usize,
    new: usize,
}

impl<'patch> Chunk<'patch> {
    fn is_empty(&self) -> bool {
        self.old_lines.is_empty() && self.new_lines.is_empty()
    }

    fn push_context_line(&mut self, line: &'patch str) {
        self.context_lines.push(ContextLine {
            old: self.old_lines.len(),
            new: self.new_lines.len(),
        });
        self.old_lines.push(line);
        self.new_lines.push(line);
    }
}

#[derive(Clone, Copy)]
enum Mode {
    NotStarted,
    Started,
    Add,
    Delete,
    Update { header_line: usize },
    Ended,
}

struct Parser<'patch> {
    mode: Mode,
    hunks: Vec<Hunk<'patch>>,
    line_number: usize,
}

/// Parses a whole envelope. Markers may carry surrounding whitespace.
pub(super) fn parse_patch(input: &str) -> Result<PatchDocument<'_>> {
    let mut parser = Parser {
        mode: Mode::NotStarted,
        hunks: Vec::new(),
        line_number: 0,
    };
    let mut lines = input.trim().lines().peekable();
    while let Some(line) = lines.next() {
        parser.line_number += 1;
        let line = line.trim_suffix('\r');
        // Only the final marker may be indented inside an update, as upstream allows.
        if lines.peek().is_none() && line.trim() == END_PATCH {
            parser.ensure_update_is_not_empty(END_PATCH)?;
            parser.mode = Mode::Ended;
        } else {
            parser.line(line)?;
        }
    }
    if !matches!(parser.mode, Mode::Ended) {
        return Err(invalid_patch(
            "The last line of the patch must be '*** End Patch'",
        ));
    }
    if parser.hunks.is_empty() {
        return Err(invalid_patch("the patch contains no file operations"));
    }
    Ok(PatchDocument {
        hunks: parser.hunks,
    })
}

impl<'patch> Parser<'patch> {
    fn line(&mut self, line: &'patch str) -> Result<()> {
        let trimmed = line.trim();
        match self.mode {
            Mode::NotStarted if trimmed == BEGIN_PATCH => {
                self.mode = Mode::Started;
                Ok(())
            }
            Mode::NotStarted => Err(invalid_patch(
                "The first line of the patch must be '*** Begin Patch'",
            )),
            Mode::Started | Mode::Delete => {
                if self.header(trimmed)? {
                    return Ok(());
                }
                Err(self.invalid_header(trimmed))
            }
            Mode::Add => {
                if self.header(trimmed)? {
                    return Ok(());
                }
                if let Some(added) = line.strip_prefix('+')
                    && let Some(Hunk::Add { contents, .. }) = self.hunks.last_mut()
                {
                    contents.push_str(added);
                    contents.push('\n');
                    return Ok(());
                }
                Err(self.invalid_header(trimmed))
            }
            Mode::Update { .. } => {
                let update_line = line.trim_end();
                if self.header(update_line)? {
                    return Ok(());
                }
                self.update_line(line, update_line)
            }
            Mode::Ended if trimmed.is_empty() => Ok(()),
            Mode::Ended => Err(invalid_patch(
                "The last line of the patch must be '*** End Patch'",
            )),
        }
    }

    /// Handles a file header or the end marker; returns whether `text` was one.
    fn header(&mut self, text: &'patch str) -> Result<bool> {
        let (hunk, mode) = if text == END_PATCH {
            self.ensure_update_is_not_empty(text)?;
            self.mode = Mode::Ended;
            return Ok(true);
        } else if let Some(path) = text.strip_prefix(ADD_FILE) {
            let hunk = Hunk::Add {
                path,
                contents: String::new(),
            };
            (hunk, Mode::Add)
        } else if let Some(path) = text.strip_prefix(DELETE_FILE) {
            (Hunk::Delete { path }, Mode::Delete)
        } else if let Some(path) = text.strip_prefix(UPDATE_FILE) {
            let header_line = self.line_number;
            let hunk = Hunk::Update {
                path,
                move_path: None,
                chunks: Vec::new(),
            };
            (hunk, Mode::Update { header_line })
        } else {
            return Ok(false);
        };
        self.ensure_update_is_not_empty(text)?;
        self.hunks.push(hunk);
        self.mode = mode;
        Ok(true)
    }

    fn ensure_update_is_not_empty(&self, line: &str) -> Result<()> {
        let (Some(Hunk::Update { path, chunks, .. }), Mode::Update { header_line }) =
            (self.hunks.last(), self.mode)
        else {
            return Ok(());
        };
        if chunks.is_empty() {
            return Err(invalid_hunk(
                header_line,
                &format!("Update file hunk for path '{path}' is empty"),
            ));
        }
        if chunks.last().is_some_and(Chunk::is_empty) {
            if line == END_PATCH {
                return Err(invalid_hunk(
                    self.line_number,
                    "Update hunk does not contain any lines",
                ));
            }
            return Err(self.unexpected_line(line));
        }
        Ok(())
    }

    fn update_line(&mut self, line: &'patch str, update_line: &'patch str) -> Result<()> {
        let line_number = self.line_number;
        let Some(Hunk::Update {
            move_path, chunks, ..
        }) = self.hunks.last_mut()
        else {
            return Err(self.unexpected_line(line));
        };
        let is_context_marker = update_line == EMPTY_CONTEXT || update_line.starts_with(CONTEXT);
        if chunks.last().is_some_and(|chunk| chunk.is_end_of_file) {
            if update_line.is_empty() {
                return Ok(());
            }
            if !is_context_marker {
                return Err(expected_context_marker(line_number, line));
            }
        }
        if chunks.is_empty()
            && move_path.is_none()
            && let Some(destination) = update_line.strip_prefix(MOVE_TO)
        {
            *move_path = Some(destination);
            return Ok(());
        }
        if is_context_marker && chunks.last().is_some_and(Chunk::is_empty) {
            return Err(unexpected_line(line_number, line));
        }
        if update_line == EMPTY_CONTEXT {
            chunks.push(Chunk::default());
            return Ok(());
        }
        if let Some(context) = update_line.strip_prefix(CONTEXT) {
            chunks.push(Chunk {
                change_context: Some(context),
                ..Chunk::default()
            });
            return Ok(());
        }
        if update_line == END_OF_FILE {
            match chunks.last_mut() {
                Some(chunk) if chunk.is_empty() => {
                    return Err(invalid_hunk(
                        line_number,
                        "Update hunk does not contain any lines",
                    ));
                }
                Some(chunk) => chunk.is_end_of_file = true,
                None => {}
            }
            return Ok(());
        }
        let (kind, text) = match line.split_at_checked(1) {
            _ if line.is_empty() => (' ', ""),
            Some((" ", text)) => (' ', text),
            Some(("+", text)) => ('+', text),
            Some(("-", text)) => ('-', text),
            _ if chunks.last().is_some_and(|chunk| !chunk.is_empty()) => {
                return Err(expected_context_marker(line_number, line));
            }
            _ => return Err(unexpected_line(line_number, line)),
        };
        if chunks.is_empty() {
            chunks.push(Chunk::default());
        }
        if let Some(chunk) = chunks.last_mut() {
            match kind {
                '+' => chunk.new_lines.push(text),
                '-' => chunk.old_lines.push(text),
                _ => chunk.push_context_line(text),
            }
        }
        Ok(())
    }

    fn invalid_header(&self, trimmed: &str) -> Error {
        invalid_hunk(
            self.line_number,
            &format!(
                "'{trimmed}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
            ),
        )
    }

    fn unexpected_line(&self, line: &str) -> Error {
        unexpected_line(self.line_number, line)
    }
}

fn unexpected_line(line_number: usize, line: &str) -> Error {
    invalid_hunk(
        line_number,
        &format!("Unexpected line found in update hunk: '{line}'. {UNEXPECTED_LINE}"),
    )
}

fn expected_context_marker(line_number: usize, line: &str) -> Error {
    invalid_hunk(
        line_number,
        &format!("Expected update hunk to start with a @@ context marker, got: '{line}'"),
    )
}

fn invalid_patch(reason: &str) -> Error {
    Error::Tool(format!(
        "Patch rejected: invalid patch: {reason}. No changes were written."
    ))
}

fn invalid_hunk(line_number: usize, message: &str) -> Error {
    Error::Tool(format!(
        "Patch rejected: invalid hunk at line {line_number}, {message}. No changes were written."
    ))
}

/// Returns `original` with `chunks` applied, charging matching work to `work`.
///
/// Unchanged lines keep their own endings; inserted lines take the file's first ending, and
/// every line ends with one, as upstream's updates always have.
pub(super) fn apply_chunks(
    original: &str,
    path: &str,
    chunks: &[Chunk<'_>],
    work: &mut usize,
) -> Result<String> {
    let source = SourceFile::parse(original);
    let lines = source.texts.as_slice();
    let mut replacements = Vec::new();
    let mut line_index = 0;
    let changes = chunks.len();
    for (number, chunk) in (1..).zip(chunks) {
        if let Some(context) = &chunk.change_context {
            let found = seek_sequence(
                lines,
                std::slice::from_ref(context),
                line_index,
                false,
                work,
            )?;
            let Some(index) = found else {
                return Err(Error::Tool(format!(
                    "Patch rejected: {path}: change {number} of {changes}: context {:?} was not found after the previous change. No changes were written.",
                    capped(context, MAX_TOOL_UI_BYTES)
                )));
            };
            line_index = index + 1;
        }
        if chunk.old_lines.is_empty() {
            replacements.push(Replacement {
                start: lines.len(),
                old_len: 0,
                new_lines: &chunk.new_lines,
            });
            continue;
        }
        let mut pattern = chunk.old_lines.as_slice();
        let mut new_lines = chunk.new_lines.as_slice();
        let mut found = seek_sequence(lines, pattern, line_index, chunk.is_end_of_file, work)?;
        // A trailing empty line stands for the final newline, which the source stores as
        // the previous line's ending.
        if found.is_none()
            && let Some((last, shorter)) = pattern.split_last()
            && last.is_empty()
        {
            pattern = shorter;
            if let Some((last, shorter)) = new_lines.split_last()
                && last.is_empty()
            {
                new_lines = shorter;
            }
            found = seek_sequence(lines, pattern, line_index, chunk.is_end_of_file, work)?;
        }
        let Some(start) = found else {
            return Err(unmatched_change(
                path, number, changes, lines, line_index, pattern, work,
            ));
        };
        // Context lines keep their original text and endings: only the runs between them
        // are replaced.
        let (mut old_start, mut new_start) = (0, 0);
        for context in &chunk.context_lines {
            if context.old >= pattern.len() || context.new >= new_lines.len() {
                break;
            }
            if old_start != context.old || new_start != context.new {
                replacements.push(Replacement {
                    start: start + old_start,
                    old_len: context.old - old_start,
                    new_lines: new_lines.get(new_start..context.new).unwrap_or_default(),
                });
            }
            old_start = context.old + 1;
            new_start = context.new + 1;
        }
        if old_start != pattern.len() || new_start != new_lines.len() {
            replacements.push(Replacement {
                start: start + old_start,
                old_len: pattern.len().saturating_sub(old_start),
                new_lines: new_lines.get(new_start..).unwrap_or_default(),
            });
        }
        line_index = start + pattern.len();
    }
    replacements.sort_by_key(|replacement| replacement.start);
    source.apply(&replacements)
}

struct Replacement<'chunk> {
    start: usize,
    old_len: usize,
    new_lines: &'chunk [&'chunk str],
}

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

/// A file's lines without their endings, and where each line starts.
struct SourceFile<'source> {
    contents: &'source str,
    texts: Vec<&'source str>,
    starts: Vec<usize>,
    preferred_ending: LineEnding,
}

impl<'source> SourceFile<'source> {
    /// Splits contents into lines; the first ending is preferred for inserted lines, and LF
    /// when there is none.
    fn parse(contents: &'source str) -> Self {
        let mut source = Self {
            contents,
            texts: Vec::new(),
            starts: Vec::new(),
            preferred_ending: LineEnding::Lf,
        };
        let mut preferred_ending = None;
        // Splitting on LF is memchr-fast; only files with a CR need the slower CR scan.
        let carriage_returns = contents.contains('\r');
        let mut start = 0;
        for piece in contents.split_inclusive('\n') {
            let (body, ending) = if let Some(body) = piece.strip_suffix("\r\n") {
                (body, Some(LineEnding::CrLf))
            } else if let Some(body) = piece.strip_suffix('\n') {
                (body, Some(LineEnding::Lf))
            } else {
                (piece, None)
            };
            if carriage_returns && body.contains('\r') {
                let mut line_start = start;
                let mut parts = body.split('\r').peekable();
                while let Some(text) = parts.next() {
                    let last = parts.peek().is_none();
                    if last && ending.is_none() && text.is_empty() {
                        break;
                    }
                    preferred_ending.get_or_insert(if last {
                        ending
                    } else {
                        Some(LineEnding::Cr)
                    });
                    source.texts.push(text);
                    source.starts.push(line_start);
                    line_start += text.len() + 1;
                }
            } else {
                preferred_ending.get_or_insert(ending);
                source.texts.push(body);
                source.starts.push(start);
            }
            start += piece.len();
        }
        source.preferred_ending = preferred_ending.flatten().unwrap_or(LineEnding::Lf);
        source
    }

    /// The bytes of lines `from..to`, endings included.
    fn run(&self, from: usize, to: usize) -> Option<&'source str> {
        let end = self.starts.get(to).copied().unwrap_or(self.contents.len());
        self.contents.get(*self.starts.get(from)?..end)
    }

    /// Rebuilds the file from source-ordered replacements, copying unchanged runs whole.
    /// Every line ends with an ending, as upstream's updates always have.
    fn apply(&self, replacements: &[Replacement<'_>]) -> Result<String> {
        let preferred = self.preferred_ending.as_str();
        let inserted: usize = replacements
            .iter()
            .flat_map(|replacement| replacement.new_lines)
            .map(|line| line.len() + preferred.len())
            .sum();
        let mut contents = String::with_capacity(self.contents.len() + inserted + preferred.len());
        let mut index = 0;
        for replacement in replacements {
            if replacement.start < index || replacement.start > self.texts.len() {
                return Err(Error::Tool("Patch rejected: its changes overlap.".into()));
            }
            if replacement.start > index {
                contents.push_str(self.run(index, replacement.start).unwrap_or_default());
                terminate(&mut contents, preferred);
            }
            for line in replacement.new_lines {
                contents.push_str(line);
                contents.push_str(preferred);
            }
            index = replacement.start + replacement.old_len;
        }
        if index < self.texts.len() {
            contents.push_str(self.run(index, self.texts.len()).unwrap_or_default());
            terminate(&mut contents, preferred);
        }
        Ok(contents)
    }
}

/// Ends a copied run whose last line was the file's unterminated final line.
fn terminate(contents: &mut String, ending: &str) {
    if !contents.ends_with(['\n', '\r']) {
        contents.push_str(ending);
    }
}

/// Finds `pattern` in `lines` at or after `start`, trying exact lines, then ignoring trailing
/// whitespace, then surrounding whitespace, then also common Unicode punctuation. With
/// `eof`, the search starts where the pattern would end the file.
fn seek_sequence(
    lines: &[&str],
    pattern: &[&str],
    start: usize,
    eof: bool,
    work: &mut usize,
) -> Result<Option<usize>> {
    if pattern.is_empty() {
        return Ok(Some(start));
    }
    if pattern.len() > lines.len() {
        return Ok(None);
    }
    let start = if eof {
        (lines.len() - pattern.len()).max(start)
    } else {
        start
    };
    let passes: [fn(&str, &str) -> bool; 4] = [
        |line, want| line == want,
        |line, want| line.trim_end() == want.trim_end(),
        |line, want| line.trim() == want.trim(),
        |line, want| {
            line.trim()
                .chars()
                .map(normalize)
                .eq(want.trim().chars().map(normalize))
        },
    ];
    for same in passes {
        for (index, window) in lines.windows(pattern.len()).enumerate().skip(start) {
            let mut matched = true;
            for (line, want) in window.iter().zip(pattern) {
                charge_work(work, want.len().saturating_add(1))?;
                if !same(line, want) {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Ok(Some(index));
            }
        }
    }
    Ok(None)
}

/// Maps typographic dashes, quotes and spaces to ASCII so plain-ASCII patches still match.
const fn normalize(character: char) -> char {
    match character {
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => '-',
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
        | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
        | '\u{3000}' => ' ',
        other => other,
    }
}

fn charge_work(work: &mut usize, amount: usize) -> Result<()> {
    *work = work.saturating_add(amount);
    if *work > MAX_PATCH_MATCH_WORK {
        return Err(Error::Tool(
            "Patch rejected: it is too expensive to match safely. No changes were written.".into(),
        ));
    }
    Ok(())
}

/// Names the failed change and where its closest positional match first diverges.
fn unmatched_change(
    path: &str,
    number: usize,
    changes: usize,
    lines: &[&str],
    from: usize,
    expected: &[&str],
    work: &mut usize,
) -> Error {
    let head = format!(
        "Patch rejected: {path}: change {number} of {changes} did not match the file. No changes were written."
    );
    let show = |line: &str| capped(line, MAX_TOOL_UI_BYTES);
    let first = show(expected.first().copied().unwrap_or_default());
    let file = lines.get(from..).unwrap_or_default();
    let window = |start: usize| file.get(start..).unwrap_or_default().iter().zip(expected);
    if charge_work(work, file.len().saturating_mul(expected.len())).is_err() {
        return Error::Tool(format!(
            "{head}\nNo matching section was found for {first:?}."
        ));
    }
    // ponytail: positional line overlap picks the window; reordered or edited lines still score.
    let start = (0..file.len())
        .max_by_key(|&start| {
            let score = window(start).filter(|(line, want)| *line == *want).count();
            (score, Reverse(start))
        })
        .unwrap_or_default();
    if window(start).all(|(line, want)| *line != *want) {
        return Error::Tool(format!(
            "{head}\nNo similar section was found for {first:?}."
        ));
    }
    let matched = window(start)
        .take_while(|(line, want)| *line == *want)
        .count();
    let Some(want) = expected.get(matched) else {
        return Error::Tool(format!(
            "{head}\nNo matching section was found for {first:?}."
        ));
    };
    let found = file.get(start + matched).map_or_else(
        || "end of file".to_owned(),
        |found| format!("{:?}", show(found)),
    );
    Error::Tool(format!(
        "{head}\nClosest match: first {matched} line(s) match from line {}; line {} should be {:?} but the file has {found}.",
        from + start + 1,
        from + start + matched + 1,
        show(want)
    ))
}

/// The envelope as a Git-style diff before it runs. Updates carry no line numbers, so each
/// hunk is numbered past the previous one with a line between them; a delete shows no lines.
pub(super) fn proposed_diff(
    document: &PatchDocument<'_>,
) -> std::result::Result<String, fmt::Error> {
    let mut diff = String::new();
    for hunk in &document.hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                file_header(&mut diff, None, Some(path))?;
                writeln!(diff, "@@ -0,0 +1,{} @@", contents.lines().count())?;
                for line in contents.lines() {
                    writeln!(diff, "+{line}")?;
                }
            }
            Hunk::Delete { path } => file_header(&mut diff, Some(path), None)?,
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                file_header(&mut diff, Some(path), Some(move_path.unwrap_or(path)))?;
                proposed_chunks(&mut diff, chunks)?;
            }
        }
    }
    Ok(diff)
}

fn proposed_chunks(diff: &mut String, chunks: &[Chunk<'_>]) -> fmt::Result {
    let (mut old, mut new) = (1, 1);
    for chunk in chunks {
        let before = joined_lines(&chunk.old_lines);
        let after = joined_lines(&chunk.new_lines);
        let mut options = DiffOptions::new();
        options.set_context_len(chunk.old_lines.len().max(chunk.new_lines.len()));
        let patch = options.create_patch(&before, &after);
        for hunk in patch.hunks() {
            let (old_len, new_len) = (hunk.old_range().len(), hunk.new_range().len());
            let anchor = chunk.change_context.unwrap_or_default();
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
                writeln!(diff, "{sign}{}", text.trim_end_matches('\n'))?;
            }
            old += old_len + 1;
            new += new_len + 1;
        }
    }
    Ok(())
}

fn joined_lines(lines: &[&str]) -> String {
    let mut text = String::new();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// Context lines around each committed hunk, as `diff -u` keeps.
const DIFF_CONTEXT: usize = 3;

/// Appends one file's committed change as a Git-style diff. `None` stands for an absent file.
pub(super) fn committed_diff(
    diff: &mut String,
    original: Option<(&str, &str)>,
    modified: Option<(&str, &str)>,
) -> fmt::Result {
    file_header(
        diff,
        original.map(|(path, _)| path),
        modified.map(|(path, _)| path),
    )?;
    let window = ChangedWindow::new(
        original.map_or("", |(_, content)| content),
        modified.map_or("", |(_, content)| content),
    );
    let mut options = DiffOptions::new();
    options.set_context_len(DIFF_CONTEXT);
    let patch = options.create_patch(window.old, window.new);
    for hunk in patch.hunks() {
        let shift = |range: &diffy::HunkRange| HunkSpan {
            start: range.start() + window.line,
            len: range.len(),
        };
        writeln!(
            diff,
            "@@ -{} +{} @@",
            shift(&hunk.old_range()),
            shift(&hunk.new_range())
        )?;
        for line in hunk.lines() {
            let (sign, text) = match line {
                Line::Context(text) => (' ', *text),
                Line::Delete(text) => ('-', *text),
                Line::Insert(text) => ('+', *text),
            };
            match text.strip_suffix('\n') {
                Some(text) => writeln!(diff, "{sign}{text}")?,
                None => writeln!(diff, "{sign}{text}\n\\ No newline at end of file")?,
            }
        }
    }
    Ok(())
}

/// A hunk range as unified diffs print it: the length is left out when it is one.
struct HunkSpan {
    start: usize,
    len: usize,
}

impl fmt::Display for HunkSpan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.len {
            1 => write!(formatter, "{}", self.start),
            len => write!(formatter, "{},{len}", self.start),
        }
    }
}

/// The lines two texts differ in, with [`DIFF_CONTEXT`] lines around them, so a small edit to
/// a large file is diffed in its neighbourhood only.
struct ChangedWindow<'text> {
    old: &'text str,
    new: &'text str,
    /// Lines before the window, which shift its hunks back to file line numbers.
    line: usize,
}

impl<'text> ChangedWindow<'text> {
    fn new(old: &'text str, new: &'text str) -> Self {
        let prefix = common_prefix(old.as_bytes(), new.as_bytes());
        let mut start = old
            .get(..prefix)
            .and_then(|head| head.rfind('\n'))
            .map_or(0, |end| end + 1);
        for _ in 0..DIFF_CONTEXT {
            let Some(head) = start.checked_sub(1).and_then(|end| old.get(..end)) else {
                break;
            };
            start = head.rfind('\n').map_or(0, |end| end + 1);
        }
        let (old_tail, new_tail) = (old.as_bytes().get(start..), new.as_bytes().get(start..));
        let suffix = common_suffix(old_tail.unwrap_or_default(), new_tail.unwrap_or_default());
        // The shared suffix starts after a line ending in both texts.
        let mut old_end = old.len() - suffix;
        if old_end > start && old.as_bytes().get(old_end - 1) != Some(&b'\n') {
            old_end = old
                .as_bytes()
                .get(old_end..)
                .and_then(|rest| rest.iter().position(|byte| *byte == b'\n'))
                .map_or(old.len(), |end| old_end + end + 1);
        }
        for _ in 0..DIFF_CONTEXT {
            let Some(rest) = old.get(old_end..).filter(|rest| !rest.is_empty()) else {
                break;
            };
            old_end += rest.find('\n').map_or(rest.len(), |end| end + 1);
        }
        let new_end = new.len() - (old.len() - old_end);
        let line = old
            .get(..start)
            .map_or(0, |head| head.matches('\n').count());
        match (old.get(start..old_end), new.get(start..new_end)) {
            (Some(old_window), Some(new_window)) => Self {
                old: old_window,
                new: new_window,
                line,
            },
            _ => Self { old, new, line: 0 },
        }
    }
}

/// Length of the common prefix, compared a cache line at a time.
fn common_prefix(left: &[u8], right: &[u8]) -> usize {
    const CHUNK: usize = 64;
    let whole = left
        .as_chunks::<CHUNK>()
        .0
        .iter()
        .zip(right.as_chunks::<CHUNK>().0)
        .take_while(|(left, right)| left == right)
        .count()
        * CHUNK;
    let (left, right) = (left.get(whole..), right.get(whole..));
    let rest = left
        .unwrap_or_default()
        .iter()
        .zip(right.unwrap_or_default())
        .take_while(|(left, right)| left == right)
        .count();
    whole + rest
}

/// Length of the common suffix, at most the shorter text.
fn common_suffix(left: &[u8], right: &[u8]) -> usize {
    const CHUNK: usize = 64;
    let whole = left
        .as_rchunks::<CHUNK>()
        .1
        .iter()
        .rev()
        .zip(right.as_rchunks::<CHUNK>().1.iter().rev())
        .take_while(|(left, right)| left == right)
        .count()
        * CHUNK;
    let left = left.get(..left.len() - whole).unwrap_or_default();
    let right = right.get(..right.len() - whole).unwrap_or_default();
    let rest = left
        .iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    whole + rest
}

fn file_header(diff: &mut String, original: Option<&str>, modified: Option<&str>) -> fmt::Result {
    git_header(diff, original, modified)?;
    match original {
        Some(path) => writeln!(diff, "--- a/{path}")?,
        None => writeln!(diff, "--- /dev/null")?,
    }
    match modified {
        Some(path) => writeln!(diff, "+++ b/{path}"),
        None => writeln!(diff, "+++ /dev/null"),
    }
}

fn git_header(diff: &mut String, original: Option<&str>, modified: Option<&str>) -> fmt::Result {
    let (Some(left), Some(right)) = (original.or(modified), modified.or(original)) else {
        return Ok(());
    };
    writeln!(diff, "diff --git a/{left} b/{right}")?;
    if let (Some(from), Some(to)) = (original, modified)
        && from != to
    {
        writeln!(diff, "rename from {from}\nrename to {to}")?;
    }
    Ok(())
}
