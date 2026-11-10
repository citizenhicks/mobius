use crate::files::{Transfer, Transfers};
use crate::{PreviewData, PreviewRequest, Update};
use mobius::SessionId;
use mobius_gateway::wire::GitDiffScope;
use std::path::Path;
use std::sync::Arc;

pub(super) async fn assert_workspace_previews(
    transfers: &Arc<Transfers>,
    session: &SessionId,
    workspace: &Path,
) {
    git(workspace, &["init", "--quiet"]);
    std::fs::write(workspace.join("main.rs"), "fn original() {}\n").unwrap();
    git(workspace, &["add", "main.rs"]);
    git(
        workspace,
        &[
            "-c",
            "user.name=Preview test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    std::fs::write(workspace.join("main.rs"), "fn changed() {}\n").unwrap();
    // Beyond one 256 KiB read, so a preview assembles several chunks.
    let bytes = vec![b'x'; 600_000];
    std::fs::write(workspace.join("large.txt"), &bytes).unwrap();
    let PreviewData::Files { files, truncated } = load(transfers, session, PreviewRequest::Files)
        .await
        .unwrap()
    else {
        panic!("file catalog expected")
    };
    assert!(!truncated);
    assert!(files.iter().any(|file| file.path == "main.rs"));
    assert!(
        files
            .iter()
            .any(|file| file.path == "large.txt" && file.size == bytes.len() as u64)
    );
    let PreviewData::File { bytes: actual, .. } =
        load(transfers, session, PreviewRequest::File("large.txt".into()))
            .await
            .unwrap()
    else {
        panic!("file bytes expected")
    };
    assert_eq!(
        actual, bytes,
        "preview must assemble multiple bounded chunks"
    );
    let absolute = workspace
        .canonicalize()
        .unwrap()
        .join("main.rs")
        .to_string_lossy()
        .into_owned();
    assert!(
        load(transfers, session, PreviewRequest::File(absolute))
            .await
            .is_ok()
    );
    assert!(
        load(
            transfers,
            session,
            PreviewRequest::File("../gateway/config.json".into())
        )
        .await
        .is_err()
    );
    // Outside the folder, the gateway's machine serves any absolute path to its owner.
    assert!(
        load(
            transfers,
            session,
            PreviewRequest::File("/etc/hosts".into())
        )
        .await
        .is_ok()
    );
    let PreviewData::Diff { scope, text } = load(
        transfers,
        session,
        PreviewRequest::Diff(GitDiffScope::Unstaged),
    )
    .await
    .unwrap() else {
        panic!("diff expected")
    };
    assert_eq!(scope, GitDiffScope::Unstaged);
    assert!(text.contains("-fn original() {}"));
    assert!(text.contains("+fn changed() {}"));
}

async fn load(
    transfers: &Arc<Transfers>,
    session: &SessionId,
    request: PreviewRequest,
) -> Result<PreviewData, String> {
    let result = Transfer::Preview {
        id: "preview".into(),
        session_id: session.share(),
        request,
    }
    .run(transfers)
    .await
    .unwrap()
    .unwrap();
    let Update::PreviewLoaded {
        id,
        session_id,
        result,
        ..
    } = result
    else {
        panic!("preview expected")
    };
    assert_eq!(&*id, "preview");
    assert_eq!(&session_id, session);
    result
}

fn git(workspace: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
