use super::{DiffDoc, GitDiffScope};
use gpui::{AppContext, TestAppContext};

const PATCH: &str = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n-old\n+new\n\
                    diff --git a/./src/lib.rs b/./src/lib.rs\n--- a/./src/lib.rs\n+++ b/./src/lib.rs\n@@ -1 +1 @@\n-before\n+after\n";

#[gpui::test]
fn selecting_a_changed_file_expands_it(cx: &mut TestAppContext) {
    let doc = cx.new(|cx| DiffDoc::new("chat".parse().unwrap(), GitDiffScope::Unstaged, PATCH, cx));
    doc.update(cx, |doc, cx| {
        doc.state
            .update(cx, |state, cx| super::set_all_collapsed(state, true, cx));
        doc.select_path("./src/lib.rs");
        assert_eq!(doc.selected_path(), Some("src/lib.rs"));
        doc.reveal_selected(cx);
        let state = doc.state.read(cx);
        assert!(!state.is_file_collapsed("./src/lib.rs"));
        assert!(state.is_file_collapsed("README.md"));
    });
}
