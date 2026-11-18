use super::*;
use crate::backend::sandbox::local::LocalSandbox;
use crate::backend::sandbox::{
    FileChange, FileChangeFailure, NetworkAccess, SandboxBackend, SandboxMode,
};
use crate::middleware::tools::patch::Hunk;
use diffy::Patch;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// Scenarios Codex applies in part but Mobius rejects before writing: a create over an
/// existing file, and a failure after an earlier operation.
const REJECTED_UPSTREAM_WRITES: [&str; 3] = [
    "010_move_overwrites_existing_destination",
    "011_add_overwrites_existing_file",
    "015_failure_after_partial_success_leaves_changes",
];

fn permissions(mode: SandboxMode) -> ToolPermissions {
    SandboxPermissions::restore(
        &"session".parse().expect("session ID"),
        mode,
        NetworkAccess::Denied,
        ["patch".into()],
    )
    .for_call("patch")
}

async fn apply(sandbox: Arc<Sandbox>, patch: &str) -> Result<String> {
    apply_with_mode(sandbox, patch, SandboxMode::WorkspaceWrite).await
}

async fn apply_with_mode(sandbox: Arc<Sandbox>, patch: &str, mode: SandboxMode) -> Result<String> {
    ApplyPatch
        .call(
            ToolContext::new(sandbox, permissions(mode), "turn"),
            serde_json::json!({ "patch": patch }),
        )
        .await
        .map(|response| response.content.text())
}

fn envelope(body: &str) -> String {
    format!("*** Begin Patch\n{body}*** End Patch\n")
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(children) = std::fs::read_dir(&directory) else {
            continue;
        };
        for child in children {
            let path = child.expect("directory entry").path();
            let relative = path.strip_prefix(root).expect("below root").to_path_buf();
            if path.is_dir() {
                entries.insert(relative, None);
                pending.push(path);
            } else {
                entries.insert(relative, Some(std::fs::read(&path).expect("read file")));
            }
        }
    }
    entries
}

fn copy_tree(from: &Path, to: &Path) {
    for (relative, contents) in snapshot(from) {
        let target = to.join(relative);
        match contents {
            None => std::fs::create_dir_all(target).expect("create directory"),
            Some(contents) => {
                std::fs::create_dir_all(target.parent().expect("parent")).expect("parent");
                std::fs::write(target, contents).expect("copy file");
            }
        }
    }
}

#[tokio::test]
async fn apply_patch_matches_the_upstream_scenarios() {
    let scenarios = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/middleware/tools/tests/apply_patch_scenarios");
    let mut directories: Vec<PathBuf> = std::fs::read_dir(&scenarios)
        .expect("scenarios")
        .map(|entry| entry.expect("scenario").path())
        .filter(|path| path.is_dir())
        .collect();
    directories.sort();
    assert_eq!(directories.len(), 25, "every upstream scenario runs");
    for scenario in directories {
        let name = scenario
            .file_name()
            .and_then(|name| name.to_str())
            .expect("name");
        let workspace = tempfile::tempdir().expect("workspace");
        copy_tree(&scenario.join("input"), workspace.path());
        let patch = std::fs::read_to_string(scenario.join("patch.txt")).expect("patch");
        let result = apply(test_sandbox_at(workspace.path()), &patch).await;
        let expected = if REJECTED_UPSTREAM_WRITES.contains(&name) {
            assert!(result.is_err(), "{name} must be rejected");
            snapshot(&scenario.join("input"))
        } else {
            snapshot(&scenario.join("expected"))
        };
        assert_eq!(snapshot(workspace.path()), expected, "{name}");
    }
}

#[test]
fn apply_patch_reports_the_failed_change_and_its_divergent_file_line() {
    for (changes, content, expected) in [
        (
            "@@\n-x = 1\n+x = 2\n@@\n elif co:\n     # second\n     # first\n-    y = 1\n+    y = 2\n",
            "x = 1\nelif co:\npad\nelif co:\n    # first\n    # second\n    y = 1\n",
            &[
                "a.py: change 2 of 2",
                "No changes were written",
                "line 5 should be \"    # second\"",
                "the file has \"    # first\"",
            ][..],
        ),
        // Line numbers are the original file's, whatever earlier changes insert.
        (
            "@@\n first\n+inserted\n@@\n second\n-wrong\n+replacement\n",
            "first\nsecond\nactual\n",
            &["line 3 should be \"wrong\"", "the file has \"actual\""][..],
        ),
    ] {
        let patch = envelope(&format!("*** Update File: a.py\n{changes}"));
        let Hunk::Update { chunks, .. } = parse_patch(&patch).expect("patch").hunks.remove(0)
        else {
            panic!("an update");
        };
        let error = apply_chunks(content, "a.py", &chunks, &mut 0)
            .expect_err("mismatch")
            .to_string();
        for fragment in expected {
            assert!(error.contains(fragment), "{fragment} in {error}");
        }
    }
}

#[test]
fn apply_patch_rejects_pathological_matching() {
    let content = "same\n".repeat(20_000);
    let mut changes = " same\n".repeat(999);
    changes.push_str("-different\n+changed\n");
    let patch = envelope(&format!("*** Update File: a\n@@\n{changes}"));
    let Hunk::Update { chunks, .. } = parse_patch(&patch).expect("patch").hunks.remove(0) else {
        panic!("an update");
    };
    let error = apply_chunks(&content, "a", &chunks, &mut 0).expect_err("too expensive");
    assert!(error.to_string().contains("too expensive"), "{error}");
}

#[tokio::test]
async fn apply_patch_uses_the_active_sandbox_policy() {
    let workspace = tempfile::tempdir().expect("workspace");
    let outside = tempfile::tempdir().expect("outside");
    let path = outside.path().join("note.txt");
    let patch_path = path.to_str().expect("UTF-8 outside path");
    std::fs::write(&path, "old\n").expect("write fixture");
    let sandbox = test_sandbox_at(workspace.path());
    let patch = envelope(&format!("*** Update File: {patch_path}\n@@\n-old\n+new\n"));

    let workspace_error = apply(Arc::clone(&sandbox), &patch)
        .await
        .expect_err("workspace policy must reject outside path");
    assert!(workspace_error.to_string().contains(patch_path));
    assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), "old\n");

    apply_with_mode(sandbox, &patch, SandboxMode::DangerFullAccess)
        .await
        .expect("full access patch");
    assert_eq!(std::fs::read_to_string(path).expect("patched"), "new\n");
}

#[tokio::test]
async fn apply_patch_rejects_forbidden_destinations_before_writing() {
    let root = tempfile::tempdir().expect("root");
    let (workspace, outside) = (root.path().join("workspace"), root.path().join("outside"));
    std::fs::create_dir_all(&workspace).expect("workspace");
    std::fs::create_dir_all(&outside).expect("outside");
    std::fs::write(workspace.join("a.txt"), "a\n").expect("source");
    std::os::unix::fs::symlink(&outside, workspace.join("link")).expect("symlink");
    let sandbox = test_sandbox_at(&workspace);
    let before = (snapshot(&workspace), snapshot(&outside));
    for (body, reason) in [
        (
            "*** Update File: a.txt\n*** Move to: ../outside/a.txt\n@@\n-a\n+b\n",
            "outside",
        ),
        (
            "*** Update File: a.txt\n@@\n-a\n+b\n*** Add File: link/new.txt\n+x\n",
            "symbolic link",
        ),
        (
            "*** Add File: fresh.txt\n+x\n*** Update File: a.txt\n*** Move to: ./fresh.txt\n@@\n-a\n+b\n",
            "more than one",
        ),
        (
            "*** Add File: parent\n+x\n*** Add File: parent/child\n+y\n",
            "is inside parent",
        ),
    ] {
        let error = apply(Arc::clone(&sandbox), &envelope(body))
            .await
            .expect_err("forbidden destination")
            .to_string();
        assert!(error.contains(reason), "{reason} in {error}");
        assert!(error.contains("No changes were written"), "{error}");
        assert_eq!((snapshot(&workspace), snapshot(&outside)), before);
    }
}

type BeforeApply = dyn Fn(&FileChange) -> Option<BoxFuture<'static, Result<()>>> + Send + Sync;

/// Delegates to a local sandbox, running `before_apply` ahead of each applied change.
struct Interposed {
    local: LocalSandbox,
    before_apply: Box<BeforeApply>,
}

impl SandboxBackend for Interposed {
    fn read<'a>(&'a self, path: &'a str, mode: SandboxMode) -> BoxFuture<'a, Result<String>> {
        self.local.read(path, mode)
    }

    fn read_bytes<'a>(
        &'a self,
        _: &'a str,
        _: usize,
        _: SandboxMode,
    ) -> BoxFuture<'a, Result<Vec<u8>>> {
        Box::pin(async { unreachable!() })
    }

    fn write<'a>(&'a self, _: &'a str, _: &'a str, _: SandboxMode) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { unreachable!() })
    }

    fn check_file_change<'a>(
        &'a self,
        change: &'a FileChange,
        mode: SandboxMode,
    ) -> BoxFuture<'a, Result<()>> {
        self.local.check_file_change(change, mode)
    }

    fn apply_file_change(
        &self,
        change: FileChange,
        mode: SandboxMode,
    ) -> BoxFuture<'_, std::result::Result<(), FileChangeFailure>> {
        Box::pin(async move {
            if let Some(interposed) = (self.before_apply)(&change) {
                interposed.await?;
            }
            self.local.apply_file_change(change, mode).await
        })
    }

    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: SandboxMode,
        _: NetworkAccess,
        _: crate::backend::sandbox::CommandMode,
        _: crate::backend::sandbox::CommandOutputSink,
    ) -> BoxFuture<'a, Result<crate::backend::sandbox::CommandOutput>> {
        Box::pin(async { unreachable!() })
    }
}

fn interposed_sandbox(
    root: &Path,
    before_apply: impl Fn(&FileChange) -> Option<BoxFuture<'static, Result<()>>> + Send + Sync + 'static,
) -> Arc<Sandbox> {
    Arc::new(Sandbox::new(
        Arc::new(Interposed {
            local: LocalSandbox::new(root).expect("sandbox"),
            before_apply: Box::new(before_apply),
        }),
        crate::backend::sandbox::ApprovalPolicy::Ask,
    ))
}

fn change_path(change: &FileChange) -> &str {
    match change {
        FileChange::Create { path, .. }
        | FileChange::Update { path, .. }
        | FileChange::Delete { path, .. } => path,
    }
}

const THREE_FILES: &str = "*** Update File: first.txt\n@@\n-one\n+ONE\n\
    *** Update File: second.txt\n@@\n-two\n+TWO\n\
    *** Add File: third.txt\n+three\n";

#[tokio::test]
async fn apply_patch_keeps_a_concurrent_edit_and_reports_each_operation() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("first.txt"), "one\n").expect("first");
    std::fs::write(workspace.path().join("second.txt"), "two\n").expect("second");
    let second = workspace.path().join("second.txt");
    // Someone edits second.txt while first.txt is being written.
    let sandbox = interposed_sandbox(workspace.path(), move |change| {
        (change_path(change) == "first.txt").then(|| {
            std::fs::write(&second, "theirs\n").expect("concurrent edit");
            Box::pin(async { Ok(()) }) as BoxFuture<'static, Result<()>>
        })
    });

    let report = apply(sandbox, &envelope(THREE_FILES))
        .await
        .expect_err("stale second file")
        .to_string();

    for fragment in [
        "operation 2 of 3 (update second.txt)",
        "changed after the edit was prepared",
        "do not resend these:\n- update first.txt",
        "Not attempted:\n- add third.txt",
        "+++ b/first.txt",
    ] {
        assert!(report.contains(fragment), "{fragment} in {report}");
    }
    let read = |name| std::fs::read_to_string(workspace.path().join(name)).ok();
    assert_eq!(read("first.txt").as_deref(), Some("ONE\n"));
    assert_eq!(read("second.txt").as_deref(), Some("theirs\n"));
    assert_eq!(read("third.txt"), None);
}

#[tokio::test]
async fn apply_patch_stops_when_cancelled() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("first.txt"), "one\n").expect("first");
    std::fs::write(workspace.path().join("second.txt"), "two\n").expect("second");
    let sandbox = interposed_sandbox(workspace.path(), |change| {
        (change_path(change) == "second.txt")
            .then(|| Box::pin(std::future::pending()) as BoxFuture<'static, Result<()>>)
    });

    let patch = envelope(THREE_FILES);
    let call = apply(sandbox, &patch);
    tokio::time::timeout(std::time::Duration::from_millis(200), call)
        .await
        .expect_err("the second change never finishes");

    let read = |name| std::fs::read_to_string(workspace.path().join(name)).ok();
    assert_eq!(read("first.txt").as_deref(), Some("ONE\n"));
    assert_eq!(read("second.txt").as_deref(), Some("two\n"));
    assert_eq!(
        read("third.txt"),
        None,
        "nothing after the cancelled change runs"
    );
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn apply_patch_reports_an_input_output_failure_after_earlier_writes() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("first.txt"), "one\n").expect("first");
    let locked = workspace.path().join("locked");
    std::fs::create_dir(&locked).expect("locked");
    set_mode(&locked, 0o555);
    let patch = envelope(
        "*** Update File: first.txt\n@@\n-one\n+ONE\n*** Add File: locked/new.txt\n+x\n*** Add File: after.txt\n+y\n",
    );

    let report = apply(test_sandbox_at(workspace.path()), &patch)
        .await
        .expect_err("read-only directory")
        .to_string();
    set_mode(&locked, 0o755);

    assert!(
        report.contains("operation 2 of 3 (add locked/new.txt)"),
        "{report}"
    );
    assert!(report.contains("- update first.txt"), "{report}");
    assert!(
        report.contains("Not attempted:\n- add after.txt"),
        "{report}"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("first.txt")).expect("first"),
        "ONE\n"
    );
    assert!(!workspace.path().join("after.txt").exists());
}

#[tokio::test]
async fn apply_patch_reports_a_move_whose_source_remains() {
    let workspace = tempfile::tempdir().expect("workspace");
    let source = workspace.path().join("old");
    std::fs::create_dir(&source).expect("source directory");
    std::fs::write(source.join("name.txt"), "old\n").expect("source");
    set_mode(&source, 0o555);

    let report = apply(
        test_sandbox_at(workspace.path()),
        &envelope("*** Update File: old/name.txt\n*** Move to: new/name.txt\n@@\n-old\n+new\n"),
    )
    .await
    .expect_err("source cannot be removed")
    .to_string();
    set_mode(&source, 0o755);

    assert!(
        report.contains(
            "new/name.txt was created with the new contents, but its source was not removed"
        ),
        "{report}"
    );
    assert!(!report.contains("No changes were written"), "{report}");
    let read = |name| std::fs::read_to_string(workspace.path().join(name)).expect(name);
    assert_eq!(read("new/name.txt"), "new\n");
    assert_eq!(read("old/name.txt"), "old\n");
}

#[tokio::test]
async fn apply_patch_keeps_permissions_through_updates_and_moves() {
    let workspace = tempfile::tempdir().expect("workspace");
    for name in ["run.sh", "tool.sh"] {
        let path = workspace.path().join(name);
        std::fs::write(&path, "echo old\n").expect("script");
        set_mode(&path, 0o751);
    }
    apply(
        test_sandbox_at(workspace.path()),
        &envelope(
            "*** Update File: run.sh\n@@\n-echo old\n+echo new\n\
             *** Update File: tool.sh\n*** Move to: bin/tool.sh\n@@\n-echo old\n+echo new\n",
        ),
    )
    .await
    .expect("patch");

    assert_eq!(mode(&workspace.path().join("run.sh")), 0o751);
    assert_eq!(mode(&workspace.path().join("bin/tool.sh")), 0o751);
}

#[tokio::test]
async fn apply_patch_renders_every_committed_file_as_a_linked_diff() {
    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("edit.txt"), "a\nb\n").expect("edit");
    std::fs::write(workspace.path().join("gone.txt"), "bye\n").expect("gone");
    std::fs::write(workspace.path().join("from.txt"), "x\n").expect("from");
    let patch = envelope(
        "*** Add File: new.txt\n+hello\n*** Update File: edit.txt\n@@\n a\n-b\n+B\n\
         *** Delete File: gone.txt\n*** Update File: from.txt\n*** Move to: to.txt\n@@\n-x\n+y\n",
    );

    let diff = apply(test_sandbox_at(workspace.path()), &patch)
        .await
        .expect("patch");

    let sections = diff.lines().filter(|line| line.starts_with("diff --git "));
    assert_eq!(sections.count(), 4, "{diff}");
    for header in [
        "diff --git a/new.txt b/new.txt\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n",
        "--- a/edit.txt\n+++ b/edit.txt\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n",
        "--- a/gone.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n",
        "rename from from.txt\nrename to to.txt\n--- a/from.txt\n+++ b/to.txt\n",
    ] {
        assert!(diff.contains(header), "{header} in {diff}");
    }
    let block = ApplyPatch
        .render(&EventMsg::ToolCallEnd(crate::protocol::ToolCallEndEvent {
            turn_id: "turn".into(),
            call_id: "patch".into(),
            name: "apply_patch".into(),
            output: diff.into(),
            is_error: false,
        }))
        .expect("patch result");
    assert_eq!(block.update, crate::protocol::FrontendBlockUpdate::Replace);
    let labels: Vec<&str> = block.links.iter().map(|link| link.label.as_str()).collect();
    assert_eq!(labels, ["new.txt", "edit.txt", "to.txt"]);
}

#[test]
fn apply_patch_begin_renders_its_envelope_as_a_linked_unified_diff() {
    let block = ApplyPatch
        .render(&EventMsg::ToolCallBegin(crate::protocol::ToolCallBeginEvent {
            turn_id: "turn".into(),
            call_id: "patch".into(),
            name: "apply_patch".into(),
            arguments: serde_json::json!({
                "patch": "*** Begin Patch\n*** Update File: src/a b.rs\n@@ fn x\n-a\n+b\n c\n@@\n+d\n*** Add File: new.rs\n+n\n*** End Patch"
            }),
        }))
        .expect("patch heading");
    assert_eq!(
        block.format,
        crate::protocol::FrontendBlockFormat::UnifiedDiff
    );
    assert_eq!(
        block.text,
        "diff --git a/src/a b.rs b/src/a b.rs\n--- a/src/a b.rs\n+++ b/src/a b.rs\n@@ -1,2 +1,2 @@ fn x\n-a\n+b\n c\n@@ -4,0 +4,1 @@\n+d\n\
         diff --git a/new.rs b/new.rs\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,1 @@\n+n\n"
    );
    let labels: Vec<&str> = block.links.iter().map(|link| link.label.as_str()).collect();
    assert_eq!(labels, ["a b.rs", "new.rs"]);
    for section in block.text.split("diff --git ").skip(1) {
        let section = format!("diff --git {section}");
        assert!(Patch::from_str(&section).is_ok(), "{section}");
    }
}

#[test]
fn apply_patch_approval_previews_the_diff_its_tool_renders() {
    let stack =
        crate::middleware::MiddlewareStack::new(vec![Arc::new(Tools::new(vec![Arc::new(
            ApplyPatch,
        )]))])
        .expect("middleware stack");
    let frontend = crate::middleware::FrontendExtensions::new(
        Arc::new(stack),
        "session".parse().expect("session ID"),
    )
    .expect("frontend");
    let arguments = serde_json::json!({
        "patch": "*** Begin Patch\n*** Update File: a.rs\n@@\n-a\n+b\n*** End Patch"
    });
    let mut request = crate::protocol::ExecApprovalRequestEvent {
        id: "request".into(),
        turn_id: "turn".into(),
        calls: vec![crate::protocol::ApprovalCall {
            call_id: "patch".into(),
            name: "apply_patch".into(),
            arguments: arguments.clone(),
            preview: None,
        }],
        reason: String::new(),
    };
    frontend.preview_approvals(&mut request);
    let call = &request.calls[0];
    let preview = call.preview.as_ref().expect("approval preview");
    assert_eq!(
        preview.format,
        crate::protocol::FrontendBlockFormat::UnifiedDiff
    );
    assert_eq!(call.arguments, arguments);
}

#[tokio::test]
async fn apply_patch_bounds_the_contents_it_prepares() {
    let workspace = tempfile::tempdir().expect("workspace");
    let mut body = String::new();
    for index in 0..18 {
        let name = format!("{index}.txt");
        std::fs::write(workspace.path().join(&name), "x".repeat(1_000_000)).expect("file");
        writeln!(body, "*** Delete File: {name}").expect("write to String");
    }

    let error = apply(test_sandbox_at(workspace.path()), &envelope(&body))
        .await
        .expect_err("too much to prepare")
        .to_string();

    assert!(error.contains("smaller patches"), "{error}");
    assert_eq!(snapshot(workspace.path()).len(), 18, "nothing was deleted");
}
