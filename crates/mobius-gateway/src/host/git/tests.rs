use super::*;

#[test]
fn a_cut_diff_keeps_a_valid_last_hunk() {
    let mut diff = b"--- a/x\n+++ b/x\n@@ -1,40 +1,40 @@ fn main\n".to_vec();
    for line in 0..40 {
        diff.extend_from_slice(format!("-old {line}\n+new {line}\n").as_bytes());
    }
    assert!(truncate_diff(&mut diff, 200));
    let text = String::from_utf8(diff).unwrap();
    let count = |sign| {
        text.lines()
            .skip(3)
            .filter(|line| line.starts_with(sign))
            .count()
    };
    let header = format!("@@ -1,{} +1,{} @@ fn main\n", count('-'), count('+'));
    assert!(text.contains(&header), "{text}");
    assert!(text.len() <= 200);

    let complete = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
    let partial = "diff --git a/y b/y\n--- a/y\n+++ b/y\n";
    let mut diff = format!("{complete}{partial}{}", "metadata".repeat(30)).into_bytes();
    assert!(truncate_diff(
        &mut diff,
        complete.len() + partial.len() + 64
    ));
    assert_eq!(String::from_utf8(diff).unwrap(), complete);
}

#[test]
fn git_credential_protocol_accepts_https_and_rejects_injection() {
    assert_eq!(
        credential_target("git.example.com/team/repo").expect("target"),
        "https://git.example.com/team/repo"
    );
    assert!(credential_target("http://git.example.com").is_err());
    assert!(credential_target("https://user:token@git.example.com").is_err());
    assert!(credential_target("git.example.com\npassword=leak").is_err());
    assert!(credential_field("name\npassword=leak", 512, "username").is_err());
    assert!(credential_field("name\twith-control", 512, "username").is_err());

    let input = credential_input("https://git.example.com/", Some("octo"), Some("token"));
    assert_eq!(
        input,
        b"url=https://git.example.com/\nusername=octo\npassword=token\n\n"
    );

    let mut output =
        b"protocol=https\nhost=git.example.com\nusername=octo\npassword=x\n\n".to_vec();
    let username = parse_credential_username("https://git.example.com/", &output)
        .expect("credential username");
    output.fill(0);
    assert_eq!(username, "octo");
    assert!(
        parse_credential_username(
            "https://git.example.com/",
            b"protocol=https\nhost=other.example.com\nusername=octo\npassword=x\n\n"
        )
        .is_err()
    );
}

use crate::git::run_git;

use crate::host::tests::test_sandbox;

fn initialize_repository(workspace: &Path, branch: &str) {
    run_git(workspace, &["init", "--quiet", "--initial-branch", branch]);
    run_git(
        workspace,
        &["config", "user.email", "mobius@example.invalid"],
    );
    run_git(workspace, &["config", "user.name", "möbius Test"]);
    run_git(workspace, &["config", "commit.gpgsign", "false"]);
    std::fs::write(workspace.join("tracked.txt"), branch).expect("tracked file");
    run_git(workspace, &["add", "--", "tracked.txt"]);
    run_git(workspace, &["commit", "--quiet", "-m", "initial"]);
}

#[tokio::test]
async fn scoped_git_reads_preserve_diff_scopes_and_untracked_totals() {
    let workspace = tempfile::tempdir().expect("workspace");
    initialize_repository(workspace.path(), "main");
    std::fs::write(workspace.path().join("tracked.txt"), "staged\nlines\n")
        .expect("staged contents");
    run_git(workspace.path(), &["add", "--", "tracked.txt"]);
    std::fs::write(
        workspace.path().join("tracked.txt"),
        "staged\nlines\nunstaged\n",
    )
    .expect("unstaged contents");
    std::fs::write(workspace.path().join("untracked.txt"), "new\nfile\n")
        .expect("untracked contents");
    let (_state, sandbox) = test_sandbox(workspace.path(), GIT_TIMEOUT);
    for (scope, additions, deletions) in [
        (GitDiffScope::Staged, 2, 1),
        (GitDiffScope::Unstaged, 3, 0),
        (GitDiffScope::Committed, 1, 0),
    ] {
        let totals = diff_totals(&sandbox, workspace.path(), scope)
            .await
            .expect("diff totals");
        assert_eq!((totals.additions, totals.deletions), (additions, deletions));
    }
    let patch = diff(&sandbox, workspace.path(), GitDiffScope::Unstaged)
        .await
        .expect("diff preview")
        .text;
    assert!(patch.contains("+unstaged\n"), "{patch}");
    assert!(patch.contains("+new\n+file\n"), "{patch}");
}

#[tokio::test]
async fn branch_switch_disables_hooks_and_confines_filters_to_the_workspace() {
    use std::os::unix::fs::PermissionsExt as _;

    let workspace = tempfile::tempdir().expect("workspace");
    initialize_repository(workspace.path(), "main");
    run_git(workspace.path(), &["switch", "--quiet", "-c", "feature"]);
    run_git(
        workspace.path(),
        &[
            "config",
            "filter.mobius.smudge",
            "sh -c 'touch .agents/filter-ran .codex/filter-ran 2>/dev/null; cat'",
        ],
    );
    std::fs::write(
        workspace.path().join(".gitattributes"),
        "filtered.txt filter=mobius\n",
    )
    .expect("attributes");
    std::fs::write(workspace.path().join("filtered.txt"), "feature\n").expect("filtered file");
    run_git(workspace.path(), &["add", "--", "."]);
    run_git(workspace.path(), &["commit", "--quiet", "-m", "feature"]);
    run_git(workspace.path(), &["switch", "--quiet", "main"]);
    for directory in [".agents", ".codex"] {
        std::fs::create_dir(workspace.path().join(directory)).expect("workspace directory");
        std::fs::write(
            workspace.path().join(directory).join("sentinel"),
            "existing",
        )
        .expect("workspace sentinel");
    }
    let hook = workspace.path().join(".git/hooks/post-checkout");
    std::fs::write(&hook, "#!/bin/sh\ntouch hook-ran\n").expect("checkout hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
        .expect("hook permissions");
    let (_state, sandbox) = test_sandbox(workspace.path(), GIT_TIMEOUT);

    switch_branch(&sandbox, "feature")
        .await
        .expect("switch branch");
    let status = status(&sandbox).await.expect("Git status");

    assert_eq!(
        (
            status.current_branch.as_str(),
            std::fs::read_to_string(workspace.path().join("filtered.txt")).expect("filtered file"),
            workspace.path().join("hook-ran").exists(),
            workspace.path().join(".agents/filter-ran").exists(),
            workspace.path().join(".codex/filter-ran").exists(),
        ),
        ("feature", "feature\n".into(), false, true, true)
    );
}
