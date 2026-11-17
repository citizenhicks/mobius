use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt;
use std::fmt::Write as _;
use std::iter;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::patch::{Hunk, PatchDocument, apply_chunks, committed_diff, parse_patch, proposed_diff};
use super::{
    ApprovalRequirement, ExecutionMode, HookIdentity, MAX_MUTATION_BYTES, MAX_PATCH_PREPARED_BYTES,
    Tool, ToolContext, ToolExposure,
};
use crate::backend::model::ToolDefinition;
use crate::backend::sandbox::FileChange;
use crate::backend::sandbox::FileChangeFailure;
use crate::backend::sandbox::MutationPermit;
use crate::backend::sandbox::Sandbox;
use crate::backend::session_files::SessionFileStore;
use crate::protocol::{
    ContentPart, EventMsg, FrontendBlock, FrontendBlockFormat, FrontendBlockUpdate, ImageDetail,
    ToolContent, ToolResponse,
};
use crate::{BoxFuture, Error, Result};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    read_file: super::ToolSpec,
    write_file: super::ToolSpec,
    apply_patch: super::ToolSpec,
}
crate::embedded_config! { static DEFINITION: Definition = include_str!("coding.toml") => fill_limits; }

fn fill_limits(definition: &mut Definition) {
    super::fill_max_bytes(
        &mut definition.write_file.tool,
        "/properties/content/description",
        MAX_MUTATION_BYTES,
    );
    super::fill_max_bytes(
        &mut definition.apply_patch.tool,
        "/properties/patch/description",
        MAX_MUTATION_BYTES,
    );
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: Option<String>,
    file_id: Option<String>,
}

/// Reads text files and views images, by workspace path or authorized session file.
pub(super) struct ReadFile {
    pub(super) store: Arc<SessionFileStore>,
}

impl Tool for ReadFile {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.read_file.tool
    }

    fn render(&self, event: &crate::protocol::EventMsg) -> Option<crate::protocol::FrontendBlock> {
        let mut block = DEFINITION.read_file.render(event)?;
        if let EventMsg::ToolCallBegin(call) = event
            && call.arguments.get("path").and_then(Value::as_str).is_none()
            && let Some(file_id) = call.arguments.get("file_id").and_then(Value::as_str)
        {
            block.text = super::formatted_tool_text(file_id);
        }
        Some(block)
    }

    fn read_only(&self) -> bool {
        true
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }

    fn call(&self, context: ToolContext, arguments: Value) -> BoxFuture<'_, Result<ToolResponse>> {
        Box::pin(async move {
            let arguments: ReadArgs = serde_json::from_value(arguments)?;
            let session = context.permissions.session_id();
            // Models without image input see a text placeholder; the request layer swaps it in.
            let image = match (arguments.path, arguments.file_id) {
                (Some(path), None) => {
                    let bytes = context
                        .sandbox
                        .read_bytes(
                            &path,
                            crate::backend::sandbox::MAX_BINARY_FILE_BYTES,
                            &context.permissions,
                        )
                        .await?;
                    if image::guess_format(&bytes).is_err() {
                        if bytes.len() > crate::backend::sandbox::MAX_FILE_BYTES {
                            return Err(Error::Sandbox("file exceeds read limit".into()));
                        }
                        return String::from_utf8(bytes).map(Into::into).map_err(|error| {
                            Error::Sandbox(format!("{path} is not valid UTF-8: {error}"))
                        });
                    }
                    let name = std::path::Path::new(&path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .ok_or_else(|| Error::Tool("image path must name a file".into()))?;
                    self.store
                        .ingest_image(session, name.into(), bytes, ImageDetail::Auto)
                        .await?
                }
                (None, Some(file_id)) => {
                    self.store
                        .inspect_image_id(session, &file_id, ImageDetail::Auto)
                        .await?
                }
                _ => return Err(Error::Tool("provide exactly one of path or file_id".into())),
            };
            Ok(ToolResponse {
                content: ToolContent(vec![
                    ContentPart::Text {
                        text: format!(
                            "{}: {} × {} pixels; file_id={}",
                            image.file.name, image.width, image.height, image.file.id
                        ),
                    },
                    ContentPart::Image { image },
                ]),
                is_error: false,
            })
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}

pub(super) struct WriteFile;

impl Tool for WriteFile {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.write_file.tool
    }

    fn render(&self, event: &crate::protocol::EventMsg) -> Option<crate::protocol::FrontendBlock> {
        DEFINITION.write_file.render(event)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: WriteArgs = serde_json::from_value(arguments)?;
            if arguments.content.len() > MAX_MUTATION_BYTES {
                return Err(Error::Tool(format!(
                    "content exceeds {MAX_MUTATION_BYTES} bytes"
                )));
            }
            context
                .sandbox
                .write(
                    &arguments.path,
                    &arguments.content,
                    context.permissions.mutation()?,
                )
                .await?;
            Ok((format!(
                "wrote {} bytes to {}",
                arguments.content.len(),
                arguments.path
            ))
            .into())
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArgs {
    patch: String,
}

pub(super) struct ApplyPatch;

impl Tool for ApplyPatch {
    fn definition(&self) -> &Arc<ToolDefinition> {
        &DEFINITION.apply_patch.tool
    }

    fn prompt_section(&self) -> Option<&str> {
        DEFINITION.apply_patch.prompt.as_deref()
    }

    fn render(&self, event: &EventMsg) -> Option<FrontendBlock> {
        let mut block = DEFINITION.apply_patch.render(event)?;
        match event {
            EventMsg::ToolCallBegin(call) => {
                if let Some(document) = call
                    .arguments
                    .get("patch")
                    .and_then(Value::as_str)
                    .and_then(|patch| parse_patch(patch).ok())
                    && let Ok(diff) = proposed_diff(&document)
                {
                    block.text = diff;
                    block.format = FrontendBlockFormat::UnifiedDiff;
                    for hunk in &document.hunks {
                        let linked = match hunk {
                            Hunk::Add { path, .. }
                            | Hunk::Update {
                                move_path: None,
                                path,
                                ..
                            }
                            | Hunk::Update {
                                move_path: Some(path),
                                ..
                            } => path,
                            Hunk::Delete { .. } => continue,
                        };
                        block.links.extend(super::file_link(linked));
                    }
                }
            }
            EventMsg::ToolCallEnd(result) if !result.is_error => {
                let diff = result.output.text();
                // A Replace sets the links, so the patched files are linked again.
                if diff.starts_with("diff --git ") {
                    let paths = diff.lines().filter_map(|line| line.strip_prefix("+++ b/"));
                    block.links.extend(paths.filter_map(super::file_link));
                    block.update = FrontendBlockUpdate::Replace;
                    block.text = diff;
                    block.format = FrontendBlockFormat::UnifiedDiff;
                }
            }
            _ => {}
        }
        Some(block)
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    fn approval(&self) -> ApprovalRequirement {
        ApprovalRequirement::Always
    }

    fn hook_identity(&self) -> Option<HookIdentity> {
        Some(HookIdentity {
            name: "apply_patch",
            subjects: &["apply_patch", "Edit", "Write"],
        })
    }

    fn hook_input(&self, arguments: &Value) -> Value {
        serde_json::json!({
            "command": arguments.get("patch")
        })
    }

    fn rewrite_hook_input(&self, input: Value) -> Result<Value> {
        Ok(serde_json::json!({"patch": super::commands::hook_command(&input)?}))
    }

    fn call(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> BoxFuture<'_, Result<crate::protocol::ToolResponse>> {
        Box::pin(async move {
            let arguments: ApplyPatchArgs = serde_json::from_value(arguments)?;
            if arguments.patch.len() > MAX_MUTATION_BYTES {
                return Err(Error::Tool(format!(
                    "patch exceeds {MAX_MUTATION_BYTES} bytes"
                )));
            }
            let document = parse_patch(&arguments.patch)?;
            let permit = context.permissions.mutation()?;
            let plan = prepare_patch(&context, permit, document).await?;
            commit_patch(&context.sandbox, permit, plan)
                .await
                .map(Into::into)
        })
    }
}

/// One operation whose result is known and whose paths the sandbox has authorized.
struct PlannedChange {
    label: String,
    change: FileChange,
    /// Its committed diff, while the patch's diffs still fit the display budget.
    diff: Option<String>,
}

/// Parses, authorizes and computes every operation before the first write, so a patch that
/// cannot apply writes nothing. File contents held for the commit count toward
/// [`MAX_PATCH_PREPARED_BYTES`]; diffs are kept only while they fit the output budget.
async fn prepare_patch(
    context: &ToolContext,
    permit: MutationPermit<'_>,
    document: PatchDocument<'_>,
) -> Result<Vec<PlannedChange>> {
    reject_conflicts(&document)?;
    let mut work = 0;
    let mut prepared = 0;
    let mut diff_budget = Some(context.sandbox.output_limit());
    let mut plan = Vec::with_capacity(document.hunks.len());
    for (number, hunk) in (1..).zip(document.hunks) {
        let label = hunk.to_string();
        let rejected = |error: Error| {
            Error::Tool(format!(
                "Patch rejected: operation {number} ({label}): {error}. No changes were written."
            ))
        };
        let (change, diff) = match hunk {
            Hunk::Add { path, contents } => {
                hold_prepared(&mut prepared, contents.len())?;
                let diff = retained_diff(&mut diff_budget, None, Some((path, &contents)))?;
                // The change owns its paths: it moves into the sandbox's blocking task.
                let change = FileChange::Create {
                    path: path.to_owned(),
                    content: contents,
                };
                (change, diff)
            }
            // ponytail: a delete compares UTF-8 text, so binary files are refused; compare
            // file identity instead if models need to delete them.
            Hunk::Delete { path } => {
                let expected = context
                    .sandbox
                    .read(path, &context.permissions)
                    .await
                    .map_err(rejected)?;
                hold_prepared(&mut prepared, expected.len())?;
                let diff = retained_diff(&mut diff_budget, Some((path, &expected)), None)?;
                (
                    FileChange::Delete {
                        path: path.to_owned(),
                        expected,
                    },
                    diff,
                )
            }
            Hunk::Update {
                path,
                move_path,
                chunks,
            } => {
                let expected = context
                    .sandbox
                    .read(path, &context.permissions)
                    .await
                    .map_err(rejected)?;
                hold_prepared(&mut prepared, expected.len())?;
                let content = apply_chunks(&expected, path, &chunks, &mut work)?;
                hold_prepared(&mut prepared, content.len())?;
                if move_path.is_none() && content == expected {
                    return Err(Error::Tool(format!(
                        "Patch rejected: {path}: the update makes no changes. No changes were written."
                    )));
                }
                let destination = move_path.unwrap_or(path);
                let diff = retained_diff(
                    &mut diff_budget,
                    Some((path, &expected)),
                    Some((destination, &content)),
                )?;
                let change = FileChange::Update {
                    path: path.to_owned(),
                    expected,
                    content,
                    destination: move_path.map(str::to_owned),
                };
                (change, diff)
            }
        };
        context
            .sandbox
            .check_file_change(&change, permit)
            .await
            .map_err(rejected)?;
        plan.push(PlannedChange {
            label,
            change,
            diff,
        });
    }
    Ok(plan)
}

fn hold_prepared(prepared: &mut usize, bytes: usize) -> Result<()> {
    *prepared = prepared.saturating_add(bytes);
    if *prepared > MAX_PATCH_PREPARED_BYTES {
        return Err(Error::Tool(format!(
            "Patch rejected: its files exceed {MAX_PATCH_PREPARED_BYTES} bytes in all; split it into smaller patches. No changes were written."
        )));
    }
    Ok(())
}

/// Returns one operation's diff while every diff so far fits `budget`; once one does not,
/// the budget closes and no later diff is built.
fn retained_diff(
    budget: &mut Option<usize>,
    original: Option<(&str, &str)>,
    modified: Option<(&str, &str)>,
) -> Result<Option<String>> {
    let Some(remaining) = *budget else {
        return Ok(None);
    };
    let mut diff = String::new();
    committed_diff(&mut diff, original, modified).map_err(formatting)?;
    *budget = remaining.checked_sub(diff.len());
    Ok(budget.is_some().then_some(diff))
}

/// Rejects a path used by more than one operation, or one inside another operation's file:
/// each operation is computed from the tree as it was before the patch.
fn reject_conflicts(document: &PatchDocument) -> Result<()> {
    let mut seen = BTreeSet::new();
    for hunk in &document.hunks {
        let destination = match hunk {
            Hunk::Update {
                move_path: Some(destination),
                ..
            } => Some(*destination),
            _ => None,
        };
        for path in iter::once(hunk.path()).chain(destination) {
            // ponytail: lexical identity only; other spellings of one file still meet the
            // sandbox's stale-file and existing-file checks at commit.
            let key: PathBuf = Path::new(path)
                .components()
                .filter(|part| !matches!(part, Component::CurDir))
                .collect();
            if !seen.insert(key) {
                return Err(Error::Tool(format!(
                    "Patch rejected: {path} appears in more than one operation; combine its changes into one. No changes were written."
                )));
            }
        }
    }
    // Component order places a path's descendants right after it, so checking neighbours
    // finds every file that another path would need as a directory.
    let mut previous: Option<&PathBuf> = None;
    for path in &seen {
        if let Some(parent) = previous
            && path.starts_with(parent)
        {
            return Err(Error::Tool(format!(
                "Patch rejected: {} is inside {}, which the patch treats as a file. No changes were written.",
                path.display(),
                parent.display()
            )));
        }
        previous = Some(path);
    }
    Ok(())
}

/// Applies the plan in order and reports exactly what was committed. A failure stops the
/// patch: nothing is rolled back, since that could overwrite a concurrent edit.
async fn commit_patch(
    sandbox: &Sandbox,
    permit: MutationPermit<'_>,
    plan: Vec<PlannedChange>,
) -> Result<String> {
    let total = plan.len();
    let mut committed = Vec::with_capacity(total);
    let mut diff = Some(String::new());
    let mut remaining = plan.into_iter();
    while let Some(planned) = remaining.next() {
        if let Err(failure) = sandbox.apply_file_change(planned.change, permit).await {
            let unattempted: Vec<String> = remaining.map(|planned| planned.label).collect();
            let report = PatchFailure {
                total,
                committed: &committed,
                failed: &planned.label,
                unattempted: &unattempted,
                failure: &failure,
                diff: diff.as_deref(),
            };
            return Err(Error::Tool(
                report
                    .to_report(sandbox.output_limit())
                    .map_err(formatting)?,
            ));
        }
        diff = diff.zip(planned.diff).map(|(mut diff, change)| {
            diff.push_str(&change);
            diff
        });
        committed.push(planned.label);
    }
    if let Some(diff) = diff {
        return Ok(diff);
    }
    let mut summary = format!("applied {total} operation(s) (diff too large to display):");
    for label in &committed {
        write!(summary, "\n- {label}").map_err(formatting)?;
    }
    Ok(summary)
}

struct PatchFailure<'a> {
    total: usize,
    committed: &'a [String],
    failed: &'a str,
    unattempted: &'a [String],
    failure: &'a FileChangeFailure,
    diff: Option<&'a str>,
}

impl PatchFailure<'_> {
    fn to_report(&self, output_limit: usize) -> std::result::Result<String, fmt::Error> {
        let (total, failed) = (self.total, self.failed);
        let position = self.committed.len() + 1;
        let mut report = String::new();
        match self.failure {
            FileChangeFailure::Unapplied(error) => write!(
                report,
                "Patch failed at operation {position} of {total} ({failed}): {error}."
            )?,
            FileChangeFailure::SourceRemains { destination, error } => {
                write!(
                    report,
                    "Patch failed at operation {position} of {total} ({failed}): {destination} was created with the new contents, but its source was not removed: {error}. Do not resend this move; remove the source once you have checked it."
                )?;
            }
            FileChangeFailure::Uncertain(error) => {
                write!(
                    report,
                    "Patch stopped at operation {position} of {total} ({failed}), which may have been applied: {error}. Read the file before resending it."
                )?;
            }
        }
        if self.committed.is_empty() && matches!(self.failure, FileChangeFailure::Unapplied(_)) {
            report.push_str("\nNo changes were written.");
        }
        if !self.committed.is_empty() {
            report.push_str("\nCommitted before the failure; do not resend these:");
            for label in self.committed {
                write!(report, "\n- {label}")?;
            }
        }
        if !self.unattempted.is_empty() {
            report.push_str("\nNot attempted:");
            for label in self.unattempted {
                write!(report, "\n- {label}")?;
            }
        }
        if let Some(diff) = self.diff.filter(|diff| !diff.is_empty())
            && report.len() + diff.len() < output_limit
        {
            write!(report, "\nCommitted changes:\n{diff}")?;
        }
        Ok(report)
    }
}

fn formatting(error: fmt::Error) -> Error {
    Error::Internal(format!("patch report formatting failed: {error}"))
}
