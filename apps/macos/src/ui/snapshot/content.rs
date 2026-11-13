//! Content scenarios: the companion pane and its documents.

use super::*;
use crate::ui::runtime::Link;

fn repo(path: &str) -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_default().join(path)
}

/// A chat with the pane beside it.
fn beside_chat(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    this.split_content = true;
    this.page_tabs
        .retain(|page| !matches!(page, PageTab::Content(_)));
    for id in std::mem::take(&mut this.content.tabs).into_keys() {
        this.native.close_panel(&id);
    }
    this.content.navigation = Default::default();
    this.active_page = 0;
    open_chat(this, window, cx);
}

/// Two files in the full view, where the chat is the strip's first tab.
fn full_view(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    beside_chat(this, window, cx);
    this.open_local_now(repo("README.md"), false, cx);
    this.open_local_now(repo("src/ui/mod.rs"), false, cx);
    this.split_content = false;
}

/// The last commit of this repository, read locally.
pub(super) fn last_commit_diff() -> String {
    std::process::Command::new("git")
        .args(["show", "--format=", "--no-color", "HEAD"])
        .output()
        .map_or_default(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The files tracked in this repository, read locally.
fn tracked_files() -> Vec<String> {
    std::process::Command::new("git")
        .arg("ls-files")
        .output()
        .map_or_default(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
}

const CHANGES_REVIEW: &str = "diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1 @@\n\
                            -Preview a file\n+Review changes and open the file\n\
                            diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n\
                            -Changes without a navigator\n+Changes with the shared file explorer\n";

/// Clicks a real Changes header without needing a model provider or session.
fn changes_header_action(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
    action: &'static str,
) {
    beside_chat(this, window, cx);
    if this.client.gateway.is_none() {
        documents::open_demo(
            this,
            window,
            cx,
            "fixture-changes-header",
            "Review changes",
            Default::default(),
        );
        this.set_link(Link::Ready, cx);
    }
    Workspace::when_chat_ready(window, cx, move |this, window, cx| {
        let path = repo("README.md").to_string_lossy().into_owned();
        let text = format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-before\n+after\n"
        );
        this.open_diff_text(&text, true, cx);
        click_probe(
            format!("changes:{action}:0"),
            px(15.),
            MouseButton::Left,
            window,
            cx,
        );
    });
}

pub(super) fn verify_content_action(name: &str, this: &Workspace, cx: &App) {
    if name == "downloaded-file-identity" {
        let mention = this
            .active_content()
            .and_then(|tab| this.tab_mention(&tab.id, cx));
        assert!(
            matches!(mention, Some(crate::ui::chat::Mention::ChatFile(file)) if file.id == "fixture-release-notes.md"),
            "a downloaded preview must retain its original chat-file reference"
        );
        eprintln!(
            "snapshot downloaded-file-identity: downloaded preview retains its ChatFile reference"
        );
        return;
    }
    if !matches!(name, "changes-open-file" | "changes-add-message") {
        return;
    }
    assert!(
        matches!(
            this.content
                .tabs
                .get("snapshot:diff")
                .map(|tab| &tab.content),
            Some(TabContent::Diff(_))
        ),
        "The Changes tab must remain open"
    );
    let path = repo("README.md").to_string_lossy().into_owned();
    if name == "changes-open-file" {
        assert!(
            matches!(
                this.active_content().map(|tab| &tab.content),
                Some(TabContent::Files(_))
            ),
            "Open file must activate a separate Files tab"
        );
        let id = this
            .content
            .navigation
            .file_id(&panel::TabOwner::of(&this.page_tabs), path.into());
        assert!(this.content.tabs.contains_key(&id));
    } else {
        assert!(
            this.composer.read(cx).value().contains(&format!("@{path}")),
            "Add to message must preserve the file reference"
        );
    }
    eprintln!("snapshot {name}: Changes preserved; header action verified");
}

/// A side-pane tab still waiting for the gateway: the shared spinner.
fn loading_tab(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    beside_chat(this, window, cx);
    this.open_content(
        ContentTab {
            id: "fixture-loading".into(),
            title: "Voice transcript".into(),
            content: TabContent::Loading {
                request: "".into(),
                session_id: None,
            },
        },
        cx,
    );
}

fn output_chat(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    bots::media_fixture(this);
    this.split_content = true;
    documents::open_demo(
        this,
        window,
        cx,
        "fixture-output-preview",
        "Preview an output",
        Default::default(),
    );
    this.set_link(Link::Ready, cx);
}

/// Exercises the real page selection and response handlers without a gateway request.
#[expect(
    clippy::expect_used,
    reason = "the regression requires its fixture chats and file tabs"
)]
fn chat_owned_tabs(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    while let Some(id) = this.content.tabs.keys().next().map(Arc::clone) {
        this.forget_content(&id, cx);
    }
    this.content.navigation = Default::default();
    this.page_tabs = vec![PageTab::Draft];
    this.active_page = 0;
    this.split_content = true;
    documents::open_demo(
        this,
        window,
        cx,
        "fixture-tabs-a",
        "Chat A",
        Default::default(),
    );
    this.set_link(Link::Ready, cx);
    let session_a = session_id(this).expect("fixture A is selected");
    this.open_local_now(repo("README.md"), false, cx);
    let file_a = Arc::clone(&this.active_content().expect("A file").id);
    let changes: Arc<str> = "fixture-tabs-a:diff".into();
    this.open_content(
        ContentTab {
            id: Arc::clone(&changes),
            title: "Changes".into(),
            content: TabContent::Loading {
                request: "fixture-tabs-request".into(),
                session_id: Some(session_a.share()),
            },
        },
        cx,
    );
    this.open_content(
        ContentTab {
            id: "fixture-tabs-a:launcher".into(),
            title: "New tab".into(),
            content: TabContent::Launcher,
        },
        cx,
    );
    this.open_page(PageTab::Content(Arc::clone(&changes)), window, cx);
    assert_eq!(this.visible_content().count(), 3);
    this.motion.pane_shown = Some((changes.to_string(), px(500.)));

    documents::open_demo(
        this,
        window,
        cx,
        "fixture-tabs-b",
        "Chat B",
        Default::default(),
    );
    let session_b = session_id(this).expect("fixture B is selected");
    assert_eq!(this.primary_page(), Some(&PageTab::Chat(session_b.share())));
    assert!(this.active_content().is_none());
    assert_eq!(this.visible_content().count(), 0);
    assert!(
        this.pane_closing().is_none(),
        "A's closing pane must not cover B"
    );
    assert!(this.active_session_ready());
    complete_parked_changes(this, &session_a, &session_b, &changes, cx);
    this.open_local_now(repo("README.md"), false, cx);
    let file_b = Arc::clone(&this.active_content().expect("B file").id);
    assert_ne!(
        file_a, file_b,
        "the same local file needs independent chat state"
    );

    documents::open_demo(
        this,
        window,
        cx,
        "fixture-tabs-a",
        "Chat A",
        Default::default(),
    );
    assert_eq!(this.visible_content().count(), 3);
    assert_eq!(this.active_content().map(|tab| &tab.id), Some(&changes));
    assert!(matches!(
        this.active_content().map(|tab| &tab.content),
        Some(TabContent::Diff(_))
    ));
    assert!(this.content.tabs.contains_key(&file_a));
    assert!(
        this.active_session_ready(),
        "restoring a document must still select its chat"
    );
    eprintln!(
        "snapshot content-chat-owned-tabs: A's three tabs and active Changes restored; late replies isolated"
    );
}

/// Both correlation fields must match before an inactive chat's result is applied.
fn complete_parked_changes(
    this: &mut Workspace,
    session_a: &SessionId,
    session_b: &SessionId,
    changes: &str,
    cx: &mut Context<Workspace>,
) {
    for (request, session) in [
        ("fixture-tabs-request", session_b),
        ("stale-request", session_a),
    ] {
        this.preview_loaded(
            request,
            session,
            Ok(crate::gateway::PreviewData::Diff {
                scope: mobius_gateway::wire::GitDiffScope::Unstaged,
                text: "wrong chat".into(),
            }),
            cx,
        );
    }
    assert!(matches!(
        this.content.tabs.get(changes).map(|tab| &tab.content),
        Some(TabContent::Loading { .. })
    ));
    this.preview_loaded(
        "fixture-tabs-request",
        session_a,
        Ok(crate::gateway::PreviewData::Diff {
            scope: mobius_gateway::wire::GitDiffScope::Unstaged,
            text: CHANGES_REVIEW.into(),
        }),
        cx,
    );
    assert_eq!(
        this.visible_content().count(),
        0,
        "A's late result must not open in B"
    );
    assert!(this.active_content().is_none());
}

/// The machine viewer is shared, but its action target and tab title follow the ready chat.
#[expect(
    clippy::expect_used,
    reason = "the regression requires its fixture chats and shared Computer tab"
)]
fn computer_chat_context(this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    harness::fixture(this);
    documents::open_demo(
        this,
        window,
        cx,
        "fixture-computer-a",
        "Chat A",
        Default::default(),
    );
    this.set_link(Link::Ready, cx);
    let a = session_id(this).expect("fixture A is selected");
    this.open_content(
        ContentTab {
            id: "computer".into(),
            title: this.computer_title().into(),
            content: TabContent::Desktop,
        },
        cx,
    );
    let title_a = SharedString::clone(&this.active_content().expect("A Computer tab").title);
    assert_eq!(this.computer_control_session().as_ref(), Some(&a));

    documents::open_demo(
        this,
        window,
        cx,
        "fixture-computer-b",
        "Chat B",
        Default::default(),
    );
    let b = session_id(this).expect("fixture B is selected");
    let payload = this
        .client
        .state
        .session
        .as_mut()
        .expect("fixture B payload");
    Arc::make_mut(&mut Arc::make_mut(&mut payload.session).context).owner_id =
        "fixture-codex".into();
    this.open_content(
        ContentTab {
            id: "computer".into(),
            title: this.computer_title().into(),
            content: TabContent::Desktop,
        },
        cx,
    );
    assert_ne!(
        this.active_content().expect("B Computer tab").title,
        title_a
    );
    assert_eq!(this.computer_control_session().as_ref(), Some(&b));

    documents::open_demo(
        this,
        window,
        cx,
        "fixture-computer-a",
        "Chat A",
        Default::default(),
    );
    assert_eq!(this.active_content().map(|tab| &*tab.id), Some("computer"));
    assert_eq!(
        this.computer_control_session().as_ref(),
        Some(&a),
        "Take control must target the restored chat"
    );
    assert_eq!(
        this.active_content().expect("restored Computer tab").title,
        title_a
    );
    this.activate_tab(PageTab::Chat(b));
    assert!(
        this.computer_control_session().is_none(),
        "a loading chat cannot inherit the prior chat's control target"
    );
    documents::open_demo(
        this,
        window,
        cx,
        "fixture-computer-a",
        "Chat A",
        Default::default(),
    );
    eprintln!(
        "snapshot computer-chat-context: A/B/return-A control target and title follow the ready chat"
    );
}

/// A download completion uses the usual async local preview; the capture hook
/// checks its message reference after the harness has awaited the rendered frame.
#[expect(
    clippy::expect_used,
    reason = "the regression requires its fixture attachment and private preview directory"
)]
fn downloaded_file_identity(
    this: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    harness::fixture(this);
    documents::open_demo(
        this,
        window,
        cx,
        "fixture-download-identity",
        "Review chat attachment",
        Default::default(),
    );
    this.set_link(Link::Ready, cx);
    this.split_content = true;
    this.fixture_chat_files();
    let session = session_id(this).expect("fixture chat is selected");
    let file = this
        .chat_files()
        .into_iter()
        .find(|(file, _)| file.name == "release-notes.md")
        .map(|(file, _)| file.to_owned())
        .expect("fixture chat attachment");
    let path = this
        .downloads
        .as_ref()
        .expect("private fixture preview directory")
        .path()
        .join("fixture-downloaded-identity.md");
    std::fs::write(
        &path,
        "# Release notes\n\nKeep the original chat file when adding this preview to a message.\n",
    )
    .expect("fixture attachment bytes");
    this.gateway_update(
        crate::gateway::Update::Downloaded {
            endpoint: this.client.endpoint.clone(), // rules-allow: the completion records the connected fixture endpoint
            session_id: session,
            file,
            path,
        },
        window,
        cx,
    );
}

pub(super) const SCENARIOS: &[Scenario] = &[
    ("content-chat-owned-tabs", chat_owned_tabs),
    ("computer-chat-context", computer_chat_context),
    ("downloaded-file-identity", downloaded_file_identity),
    ("content-output-preview", |this, window, cx| {
        output_chat(this, window, cx);
        this.open_file_preview(
            "fixture-output-preview:image",
            "tama-moon.jpg",
            repo("assets/backgrounds/tama-moon.jpg"),
            cx,
        );
    }),
    ("motion-output-preview", |this, window, cx| {
        output_chat(this, window, cx);
        strip_after_capture(|this, _, cx| {
            this.open_file_preview(
                "fixture-output-preview:image",
                "tama-moon.jpg",
                repo("assets/backgrounds/tama-moon.jpg"),
                cx,
            );
        });
    }),
    ("changes-open-file", |this, window, cx| {
        changes_header_action(this, window, cx, "open");
    }),
    ("changes-add-message", |this, window, cx| {
        changes_header_action(this, window, cx, "add");
    }),
    ("changes-review", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_diff_text(CHANGES_REVIEW, true, cx);
        this.split_content = false;
    }),
    ("changes-review-collapsed", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_diff_text(CHANGES_REVIEW, false, cx);
        this.split_content = false;
    }),
    ("motion-changes-rail", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_diff_text(CHANGES_REVIEW, true, cx);
        watch("motion:files-rail".into());
    }),
    ("motion-changes-folder", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_diff_text(CHANGES_REVIEW, true, cx);
        watch("motion:folder-child-src".into());
    }),
    ("motion-files-rail", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_files_list(tracked_files(), &[], cx);
        this.select_files_now("README.md", false, cx);
        watch("motion:files-rail".into());
    }),
    ("motion-folder", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_files_list(
            ["src/main.rs", "src/other.rs", "z-after.txt"]
                .map(str::to_owned)
                .into(),
            &[],
            cx,
        );
        watch("motion:folder-child-src".into());
    }),
    ("content-file", |this, window, cx| {
        beside_chat(this, window, cx);
        if let Some(path) = std::env::var_os("MOBIUS_SNAPSHOT_FILE") {
            this.open_local_now(path.into(), false, cx);
        }
    }),
    ("content-code", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_local_now(repo("src/ui/mod.rs"), false, cx);
    }),
    ("content-markdown", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_local_now(repo("src/ui/mod.rs"), false, cx);
        this.open_local_now(repo("README.md"), false, cx);
    }),
    ("content-markdown-source", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_local_now(repo("README.md"), true, cx);
    }),
    ("content-diff", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_diff_text(&last_commit_diff(), true, cx);
    }),
    ("content-diff-collapsed", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_diff_text(&last_commit_diff(), false, cx);
    }),
    ("content-files-empty", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_files_list(tracked_files(), &["src", "src/ui", "src/ui/panel"], cx);
    }),
    ("content-files-code", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_files_list(tracked_files(), &[], cx);
        this.select_files_now("src/ui/panel/file_tree.rs", false, cx);
    }),
    ("content-files-markdown", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_files_list(tracked_files(), &[], cx);
        this.select_files_now("README.md", false, cx);
    }),
    ("content-files-full", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_files_list(tracked_files(), &["docs"], cx);
        this.select_files_now("src/ui/panel/source.rs", false, cx);
        this.split_content = false;
    }),
    // The composer's `@` menu over an open code tab, the chat's files and its folder.
    ("composer-mention", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_local_now(repo("src/ui/mod.rs"), false, cx);
        this.composer
            .update(cx, |input, cx| input.insert("Look at @mo", window, cx));
        this.track_mention(cx);
    }),
    // A file `@` added to the message, drawn as a link to it.
    ("composer-link", |this, window, cx| {
        beside_chat(this, window, cx);
        let file = crate::ui::chat::Mention::Path("src/ui/theme.rs".into());
        this.add_mention(file, window, cx);
        this.composer
            .update(cx, |input, cx| input.insert("sets the colours", window, cx));
    }),
    ("content-new-tab", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_launcher(cx);
    }),
    ("content-full-view", |this, window, cx| {
        full_view(this, window, cx);
    }),
    ("content-full-view-chat", |this, window, cx| {
        full_view(this, window, cx);
        this.toggle_pane(window, cx);
    }),
    ("content-full-view-collapse", |this, window, cx| {
        full_view(this, window, cx);
        this.toggle_pane(window, cx);
        this.toggle_split(window, cx);
    }),
    ("content-narrow-split", |this, window, cx| {
        window.resize(size(px(1000.), px(720.)));
        this.navigation_visible = false;
        beside_chat(this, window, cx);
        this.open_local_now(repo("src/ui/mod.rs"), false, cx);
    }),
    // Needs a live agent browser: MOBIUS_TEST_DEVTOOLS=http://127.0.0.1:<port>.
    ("content-agent-browser", |this, window, cx| {
        beside_chat(this, window, cx);
        if let Ok(origin) = std::env::var("MOBIUS_TEST_DEVTOOLS") {
            this.snapshot_agent_browser(origin, None, cx);
        }
    }),
    ("content-agent-browser-typed", |this, window, cx| {
        beside_chat(this, window, cx);
        if let Ok(origin) = std::env::var("MOBIUS_TEST_DEVTOOLS") {
            this.snapshot_agent_browser(origin, Some("typed from möbius"), cx);
        }
    }),
    // The bundled app's browser on MOBIUS_SNAPSHOT_URL; its page is native, so capture it
    // with screencapture while MOBIUS_SNAPSHOT_HOLD_MS holds the screen.
    ("content-browser", |this, window, cx| {
        beside_chat(this, window, cx);
        this.split_content = false;
        let url =
            std::env::var("MOBIUS_SNAPSHOT_URL").unwrap_or_else(|_| "https://example.com".into());
        this.snapshot_browser(&url, window, cx);
    }),
    ("content-terminal", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_terminal_in(std::env::current_dir().unwrap_or_default(), cx);
    }),
    ("content-terminal-exited", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_terminal_in(std::env::current_dir().unwrap_or_default(), cx);
        let id = this.active_content().map(|tab| Arc::clone(&tab.id));
        if let Some(id) = id {
            this.terminal_exited(&id, Some(130));
        }
    }),
    ("content-loading", loading_tab),
    ("perf-spinner", loading_tab),
    ("content-pane-hidden", |this, window, cx| {
        beside_chat(this, window, cx);
        this.open_local_now(repo("README.md"), false, cx);
        this.open_local_now(repo("docs/DESIGN.md"), false, cx);
        this.open_local_now(repo("src/ui/mod.rs"), false, cx);
        this.toggle_pane(window, cx);
    }),
];

/// A native preview keeps its final width behind the pane's animated clipping frame.
pub(super) fn verify_pane_motion(name: &str, frame: u64, cx: &App) {
    if name != "motion-output-preview" || !(2..=10).contains(&frame) || cx.reduce_motion() {
        return;
    }
    let bounds = PROBES.with(|probes| probes.borrow().get("pane:content").copied());
    assert!(
        bounds.is_some_and(|bounds| bounds.size.width >= width::DOC_MIN),
        "The image preview must remain visible without shrinking while its pane opens: {bounds:?}"
    );
    eprintln!("pane frame {frame}: {bounds:?}");
}
