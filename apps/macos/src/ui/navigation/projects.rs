//! A project's ⋯ menu in the Chats sidebar: new chat, pin, reveal, copy path and delete.
//! The gateway has no operation that moves a workspace, so a project cannot change folder.

use super::catalog::{project_sessions, workspace_name};
use super::*;
use gpui_component::menu::PopupMenu;
use std::path::Path;

/// What a project menu shows, read when it opens.
struct ProjectFacts {
    name: String,
    path: String,
    pinned: bool,
    revealable: bool,
    chats: usize,
}

#[derive(Clone, Copy)]
enum ProjectAction {
    NewChat,
    Pin(bool),
    Reveal,
    CopyPath,
    Delete,
}

impl Workspace {
    fn project_facts(&self, project: &str) -> Option<ProjectFacts> {
        let sessions = project_sessions(&self.client.state.sessions, project);
        let path = sessions
            .first()?
            .session_context
            .workspace_label
            .clone() // rules-allow: the facts outlive the borrow of the session list
            .unwrap_or_else(|| project.to_owned());
        Some(ProjectFacts {
            name: workspace_name(&path).to_owned(),
            pinned: self.project_pinned(project),
            revealable: self.client.local_gateway
                && crate::gateway::local::is_directory(Path::new(&path)),
            chats: sessions.len(),
            path,
        })
    }

    pub(super) fn project_pinned(&self, project: &str) -> bool {
        self.navigation_catalog
            .pinned_in(self.client.endpoint.as_str())
            .contains(project)
    }

    fn project_action(
        &mut self,
        project: String,
        action: ProjectAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(facts) = self.project_facts(&project) else {
            return;
        };
        match action {
            ProjectAction::NewChat => {
                self.new_chat(window, cx);
                self.draft_workspace = facts.path.into();
            }
            ProjectAction::Pin(pinned) => self.pin_project(&project, pinned, cx),
            ProjectAction::Reveal => cx.reveal_path(Path::new(&facts.path)),
            ProjectAction::CopyPath => cx.write_to_clipboard(ClipboardItem::new_string(facts.path)),
            ProjectAction::Delete => self.confirm_delete_project(project, &facts, window, cx),
        }
        cx.notify();
    }

    fn pin_project(&mut self, project: &str, pinned: bool, cx: &Context<Self>) {
        let endpoint = self.client.endpoint.as_str().to_owned();
        let catalog = &mut self.navigation_catalog;
        catalog.set_project_pinned(&endpoint, project, pinned);
        let pins = catalog.pinned_projects.clone(); // rules-allow: preferences save their own copy of the pins the catalog keeps
        self.save_preferences(
            "pinned projects",
            |preferences| preferences.pinned_projects = pins,
            cx,
        );
    }

    /// Deletes every chat in the project, pinned ones included, after one confirmation.
    fn confirm_delete_project(
        &self,
        project: String,
        facts: &ProjectFacts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sessions = project_sessions(&self.client.state.sessions, &project);
        let pinned = sessions.iter().filter(|session| session.pinned).count();
        let ids: Vec<_> = sessions.iter().map(|s| s.session_id.share()).collect();
        let (title, body, action) = delete_prompt(&facts.name, &facts.path, ids.len(), pinned);
        self.confirm_destructive(title, body, action, window, cx, move |this, window, cx| {
            if this.project_pinned(&project) {
                this.pin_project(&project, false, cx);
            }
            this.delete_chats(&ids, window, cx);
        });
    }
}

/// The delete confirmation's title, body and button for a project of `chats` chats.
fn delete_prompt(name: &str, path: &str, chats: usize, pinned: usize) -> (String, String, String) {
    let count = counted(chats, "chat", "chats");
    let pinned = match pinned {
        0 => String::new(),
        n => format!(", including {},", counted(n, "pinned chat", "pinned chats")),
    };
    (
        format!("Delete “{name}”?"),
        format!(
            "This removes its {count}{pinned} from the gateway history. \
             The folder {path} and its files are not touched."
        ),
        format!("Delete {count}"),
    )
}

/// Builds the project menu for `project`, from facts read when it opens.
pub(super) fn project_menu(
    mut menu: PopupMenu,
    view: &WeakEntity<Workspace>,
    project: &str,
    cx: &Context<PopupMenu>,
) -> PopupMenu {
    let Some(facts) = view
        .upgrade()
        .and_then(|view| view.read(cx).project_facts(project))
    else {
        return menu;
    };
    let p = palette(cx);
    let item = |label: String, glyph: &str, action: ProjectAction| {
        let project = project.to_owned();
        let run = move |this: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>| {
            this.project_action(project.clone(), action, window, cx); // rules-allow: the click handler is Fn and each action owns its project
        };
        workspace_item(label, glyph, view, run, &p)
    };
    let (pin_label, pin_glyph) = if facts.pinned {
        ("Unpin project", "hi.pushPinSlash")
    } else {
        ("Pin project", "hi.pushPin")
    };
    menu = menu
        .item(item(
            format!("New chat in {}", facts.name),
            "hi.notePencil",
            ProjectAction::NewChat,
        ))
        .item(item(
            pin_label.into(),
            pin_glyph,
            ProjectAction::Pin(!facts.pinned),
        ))
        .separator();
    if facts.revealable {
        menu = menu.item(item(
            "Reveal in Finder".into(),
            "hi.folderOpen",
            ProjectAction::Reveal,
        ));
    }
    menu.item(item("Copy path".into(), "hi.copy", ProjectAction::CopyPath))
        .separator()
        .item(item(
            format!(
                "Delete project and {}…",
                counted(facts.chats, "chat", "chats")
            ),
            "hi.trash",
            ProjectAction::Delete,
        ))
}
