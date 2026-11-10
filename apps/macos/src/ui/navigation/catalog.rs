//! The chat catalog's model: filters, organisation and grouping (SwiftUI ChatsView).
//! Everything here is hashed into the catalog region's key.

use mobius::SessionId;
use mobius_gateway::wire::{SessionActivityState, SessionRecord};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Chats shown per project before "Show more".
pub(super) const PAGE: usize = 10;

#[derive(Clone, Copy, Default, Hash, PartialEq, Eq)]
pub(in crate::ui) enum Organization {
    #[default]
    ByProject,
    Chronological,
}

impl Organization {
    pub(super) const ALL: [Self; 2] = [Self::ByProject, Self::Chronological];

    pub(super) fn title(self) -> &'static str {
        match self {
            Self::ByProject => "By project",
            Self::Chronological => "Chronological list",
        }
    }

    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::ByProject => "hi.folder",
            Self::Chronological => "hi.clock",
        }
    }
}

#[derive(Default, Hash)]
pub(in crate::ui) struct CatalogState {
    pub(super) collapsed: BTreeSet<Arc<str>>,
    pub(in crate::ui) pinned_collapsed: bool,
    pub(super) visible: BTreeMap<Arc<str>, usize>,
    pub(in crate::ui) organization: Organization,
    pub(super) attention_only: bool,
    pub(super) bot_filter: BTreeSet<Arc<str>>,
    /// Chats that finished while another chat was open.
    pub(super) unread: BTreeSet<SessionId>,
    /// Chats last seen running or awaiting approval, to notice when that changes.
    pub(super) running: BTreeSet<SessionId>,
    pub(super) awaiting: BTreeSet<SessionId>,
    pub(super) attention: BTreeMap<SessionId, u32>,
    /// Activity changes are news only once a catalog has been seen.
    pub(super) tracking: bool,
    /// Pinned project ids by gateway endpoint, kept in the preferences.
    pub(in crate::ui) pinned_projects: BTreeMap<String, BTreeSet<String>>,
    /// The row or project whose more menu is open, which keeps its button shown.
    pub(in crate::ui) menu_open: Option<String>,
}

pub(super) struct Project<'a> {
    pub(super) id: &'a str,
    pub(super) path: &'a str,
    pub(super) pinned: bool,
    pub(super) sessions: Vec<&'a SessionRecord>,
}

/// The catalog's groups in display order: Pinned, then Projects or Recent.
#[derive(Default)]
pub(super) struct Catalog<'a> {
    pub(super) pinned: Vec<&'a SessionRecord>,
    pub(super) projects: Vec<Project<'a>>,
    pub(super) recent: Vec<&'a SessionRecord>,
}

impl Catalog<'_> {
    pub(super) fn is_empty(&self) -> bool {
        self.pinned.is_empty() && self.projects.is_empty() && self.recent.is_empty()
    }
}

impl CatalogState {
    /// The state a window starts with: pinned projects come from the preferences.
    pub(in crate::ui) fn restored(preferences: &mut crate::ui::preferences::Preferences) -> Self {
        Self {
            pinned_projects: std::mem::take(&mut preferences.pinned_projects),
            ..Default::default()
        }
    }

    pub(super) fn pinned_in(&self, endpoint: &str) -> &BTreeSet<String> {
        static NONE: BTreeSet<String> = BTreeSet::new();
        self.pinned_projects.get(endpoint).unwrap_or(&NONE)
    }

    /// Pins or unpins `project` on `endpoint`, dropping an endpoint left with none.
    pub(super) fn set_project_pinned(&mut self, endpoint: &str, project: &str, pinned: bool) {
        let ids = self.pinned_projects.entry(endpoint.to_owned()).or_default();
        if pinned {
            ids.insert(project.to_owned());
        } else {
            ids.remove(project);
        }
        if ids.is_empty() {
            self.pinned_projects.remove(endpoint);
        }
    }

    pub(super) fn needs_attention(&self, session: &SessionRecord) -> bool {
        session.activity.state != SessionActivityState::Idle
            || session.activity.attention > 0
            || self.unread.contains(&session.session_id)
    }

    fn shows(&self, session: &SessionRecord) -> bool {
        (!self.attention_only || self.needs_attention(session))
            && (self.bot_filter.is_empty()
                || self.bot_filter.contains(&session.session_context.owner_id))
    }

    /// Groups the visible chats; pinned projects lead, then `current`, the open chat's workspace.
    pub(super) fn catalog<'a>(
        &self,
        sessions: impl IntoIterator<Item = &'a SessionRecord>,
        current: Option<&str>,
        pinned_projects: &BTreeSet<String>,
    ) -> Catalog<'a> {
        let mut shown: Vec<_> = sessions
            .into_iter()
            .filter(|session| self.shows(session))
            .collect();
        shown.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let (pinned, rest): (Vec<_>, Vec<_>) = shown.into_iter().partition(|s| s.pinned);
        match self.organization {
            Organization::Chronological => Catalog {
                pinned,
                recent: rest,
                ..Default::default()
            },
            Organization::ByProject => {
                let (workspace, recent) = rest
                    .into_iter()
                    .partition(|session| project_id(session).is_some());
                Catalog {
                    pinned,
                    projects: projects(workspace, current, pinned_projects),
                    recent,
                }
            }
        }
    }

    pub(super) fn visible(&self, project: &str) -> usize {
        self.visible.get(project).copied().unwrap_or(PAGE)
    }
}

/// Sessions arrive newest first; projects keep that order within, and sort by their newest.
fn projects<'a>(
    sessions: Vec<&'a SessionRecord>,
    current: Option<&str>,
    pinned: &BTreeSet<String>,
) -> Vec<Project<'a>> {
    let mut projects: Vec<Project<'a>> = Vec::new();
    for session in sessions {
        let Some(id) = project_id(session) else {
            continue;
        };
        match projects.iter_mut().find(|project| project.id == id) {
            Some(project) => project.sessions.push(session),
            None => projects.push(Project {
                id,
                path: workspace_path(session),
                pinned: pinned.contains(id),
                sessions: vec![session],
            }),
        }
    }
    // Stable: equal keys keep newest-first order.
    projects.sort_by_key(|project| (!project.pinned, current != Some(project.id)));
    projects
}

/// The project a chat belongs to: its workspace id, or its path when it has none.
pub(in crate::ui) fn project_id(session: &SessionRecord) -> Option<&str> {
    session
        .session_context
        .workspace_id
        .as_deref()
        .or(session.session_context.workspace_label.as_deref())
}

/// Every chat in `project`, whatever the filters hide, pinned chats included.
pub(super) fn project_sessions<'a>(
    sessions: &'a [SessionRecord],
    project: &str,
) -> Vec<&'a SessionRecord> {
    sessions
        .iter()
        .filter(|session| project_id(session) == Some(project))
        .collect()
}

fn workspace_path(session: &SessionRecord) -> &str {
    session
        .session_context
        .workspace_label
        .as_deref()
        .unwrap_or("Workspace")
}

pub(in crate::ui) fn workspace_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// A chat's display title: one short line, since the first message can be a whole voice
/// transcript and GPUI shapes a row's full text every frame even when it is truncated.
pub(in crate::ui) fn session_title(session: &SessionRecord) -> String {
    let text = session
        .title
        .as_deref()
        .or(session.first_user_message.as_deref())
        .unwrap_or_default();
    let line = crate::ui::primitives::single_line(text);
    if line.is_empty() {
        "New chat".into()
    } else {
        line.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, workspace: &str, updated_at: i64, pinned: bool) -> SessionRecord {
        SessionRecord {
            session_id: id.parse().unwrap(),
            session_context: mobius::protocol::SessionContext {
                owner_id: format!("bot-{id}").into(),
                workspace_id: Some(workspace.into()),
                workspace_label: Some(format!("/work/{workspace}")),
                ..Default::default()
            },
            parent_session_id: None,
            parent_sequence: None,
            sequence: 0,
            first_user_message: None,
            execution_stats: Default::default(),
            title: Some(format!("Chat {id}")),
            pinned,
            activity: Default::default(),
            created_at: 0,
            updated_at,
        }
    }

    fn ids<'a>(sessions: &[&'a SessionRecord]) -> Vec<&'a str> {
        sessions.iter().map(|s| &*s.session_id).collect()
    }

    #[test]
    fn conversations_without_workspaces_are_not_projects_or_project_deletion_targets() {
        let mut sessions = [
            session("main", "ignored", 2, false),
            session("project", "one", 1, false),
        ];
        sessions[0].session_context.workspace_id = None;
        sessions[0].session_context.workspace_label = None;
        let catalog = CatalogState::default().catalog(&sessions, None, &BTreeSet::new());
        assert_eq!(ids(&catalog.recent), ["main"]);
        assert_eq!(
            catalog
                .projects
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>(),
            ["one"]
        );
        assert!(project_sessions(&sessions, "Workspace").is_empty());
    }

    #[test]
    fn a_project_holds_every_chat_whatever_the_filters() {
        let mut sessions = [
            session("a", "one", 1, true),
            session("b", "two", 2, false),
            session("c", "one", 3, false),
            session("d", "x", 4, false),
        ];
        sessions[3].session_context.workspace_id = None;
        let state = CatalogState {
            bot_filter: BTreeSet::from(["bot-b".into()]),
            ..Default::default()
        };
        assert!(
            state
                .catalog(&sessions, None, &BTreeSet::new())
                .projects
                .len()
                == 1
        );
        assert_eq!(ids(&project_sessions(&sessions, "one")), ["a", "c"]);
        assert_eq!(ids(&project_sessions(&sessions, "/work/x")), ["d"]);
    }
}
