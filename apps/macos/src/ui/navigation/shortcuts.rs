//! The one table of app shortcuts, and the keystroke logic behind the user's overrides.

use super::menus::{
    CloseTab, Hide, HideOthers, Minimize, NewTab, NewTerminal, OpenSettings, Quit, ShowAll,
    StopTurn, ToggleSidePanel, ToggleSplit, Zoom,
};
use crate::ui::{Back, Forward, NewChat, OpenFile, Search, ToggleSide};
use gpui::{Action, Keystroke, OperatingSystem};
use std::collections::BTreeMap;

/// Keystrokes by action id; an empty keystroke removes the default.
pub(in crate::ui) type Overrides = BTreeMap<String, String>;

/// A settings group, and the menu that lists its actions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::ui) enum Area {
    General,
    File,
    View,
    Navigation,
    Chat,
    Window,
}

impl Area {
    pub(in crate::ui) const ALL: [Self; 6] = [
        Self::General,
        Self::File,
        Self::View,
        Self::Navigation,
        Self::Chat,
        Self::Window,
    ];

    pub(in crate::ui) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::File => "File",
            Self::View => "View",
            Self::Navigation => "Navigation",
            Self::Chat => "Chat",
            Self::Window => "Window",
        }
    }

    pub(super) fn menu(self) -> &'static str {
        match self {
            Self::General => "Möbius",
            Self::Navigation => "Go",
            other => other.label(),
        }
    }
}

pub(in crate::ui) struct Shortcut {
    /// Stable across releases; overrides are stored under it.
    pub id: &'static str,
    pub label: &'static str,
    pub area: Area,
    pub default: Option<&'static str>,
    /// Works in every window rather than only the workspace.
    pub app_wide: bool,
    /// Starts a new menu section.
    pub separated: bool,
    /// The command palette lists the action with this glyph.
    pub palette: Option<&'static str>,
    pub action: fn() -> Box<dyn Action>,
}

const fn entry(
    id: &'static str,
    label: &'static str,
    area: Area,
    default: Option<&'static str>,
    action: fn() -> Box<dyn Action>,
) -> Shortcut {
    Shortcut {
        id,
        label,
        area,
        default,
        app_wide: false,
        separated: false,
        palette: None,
        action,
    }
}

impl Shortcut {
    const fn app_wide(mut self) -> Self {
        self.app_wide = true;
        self
    }

    const fn separated(mut self) -> Self {
        self.separated = true;
        self
    }

    const fn palette(mut self, glyph: &'static str) -> Self {
        self.palette = Some(glyph);
        self
    }

    /// The default keystroke in canonical form.
    pub(in crate::ui) fn default_keystroke(&self) -> Option<String> {
        self.default.and_then(canonical)
    }
}

use Area::*;

pub(in crate::ui) const SHORTCUTS: &[Shortcut] = &[
    entry(
        "check_updates",
        "Check for Updates…",
        General,
        None,
        || Box::new(super::CheckForUpdates),
    )
    .palette("hi.download01"),
    entry("settings", "Settings…", General, Some("cmd-,"), || {
        Box::new(OpenSettings)
    })
    .palette("hi.gear"),
    entry("hide", "Hide Möbius", General, Some("cmd-h"), || {
        Box::new(Hide)
    })
    .app_wide()
    .separated(),
    entry(
        "hide_others",
        "Hide Others",
        General,
        Some("alt-cmd-h"),
        || Box::new(HideOthers),
    )
    .app_wide(),
    entry("show_all", "Show All", General, None, || Box::new(ShowAll)).app_wide(),
    entry("quit", "Quit Möbius", General, Some("cmd-q"), || {
        Box::new(Quit)
    })
    .app_wide()
    .separated(),
    entry("new_chat", "New Chat", File, Some("cmd-n"), || {
        Box::new(NewChat)
    })
    .palette("hi.notePencil"),
    entry("new_tab", "New Tab", File, Some("cmd-t"), || {
        Box::new(NewTab)
    })
    .palette("hi.plus"),
    entry("new_terminal", "New Terminal", File, Some("ctrl-`"), || {
        Box::new(NewTerminal)
    })
    .palette("hi.terminalWindow"),
    entry("open_file", "Open File…", File, Some("cmd-o"), || {
        Box::new(OpenFile)
    })
    .palette("hi.folderOpen"),
    entry("close_tab", "Close Tab", File, Some("cmd-w"), || {
        Box::new(CloseTab)
    })
    .separated(),
    entry(
        "toggle_sidebar",
        "Toggle Sidebar",
        View,
        Some("ctrl-cmd-s"),
        || Box::new(ToggleSide),
    )
    .palette("hi.menu"),
    entry(
        "toggle_split",
        "Toggle Split View",
        View,
        Some("shift-cmd-b"),
        || Box::new(ToggleSplit),
    ),
    entry(
        "toggle_side_panel",
        "Toggle Side Panel",
        View,
        Some("alt-cmd-b"),
        || Box::new(ToggleSidePanel),
    ),
    entry("back", "Back", Navigation, Some("cmd-["), || Box::new(Back)),
    entry("forward", "Forward", Navigation, Some("cmd-]"), || {
        Box::new(Forward)
    }),
    entry("search", "Search…", Navigation, Some("cmd-k"), || {
        Box::new(Search)
    })
    .separated(),
    entry("stop_turn", "Stop", Chat, Some("cmd-."), || {
        Box::new(StopTurn)
    }),
    entry("minimize", "Minimize", Window, Some("cmd-m"), || {
        Box::new(Minimize)
    }),
    entry("zoom", "Zoom", Window, None, || Box::new(Zoom)),
];

pub(in crate::ui) fn find(id: &str) -> Option<&'static Shortcut> {
    SHORTCUTS.iter().find(|shortcut| shortcut.id == id)
}

/// One spelling per keystroke, so `shift-cmd-b` and `cmd-shift-b` compare equal.
pub(in crate::ui) fn canonical(source: &str) -> Option<String> {
    if source.contains(char::is_whitespace) {
        return None;
    }
    Keystroke::parse_with_os(source, OperatingSystem::Mac)
        .ok()
        .filter(|keystroke| !keystroke.key.is_empty())
        .map(|keystroke| keystroke.unparse(OperatingSystem::Mac))
}

/// A pressed keystroke usable as a shortcut: it needs ⌘, ⌃ or ⌥ unless it is a function key,
/// so shortcuts never swallow typing.
pub(in crate::ui) fn recordable(keystroke: &Keystroke) -> Option<String> {
    let modifiers = keystroke.modifiers;
    let function_key = keystroke
        .key
        .strip_prefix('f')
        .is_some_and(|number| number.parse::<u8>().is_ok());
    (modifiers.control || modifiers.alt || modifiers.platform || function_key)
        .then(|| {
            Keystroke::parse_with_os(
                &keystroke.unparse(OperatingSystem::Mac),
                OperatingSystem::Mac,
            )
            .ok()
        })
        .flatten()
        .map(|keystroke| keystroke.unparse(OperatingSystem::Mac))
}

/// The keystroke `shortcut` answers to after `overrides`; an unreadable override keeps the default.
pub(in crate::ui) fn binding(overrides: &Overrides, shortcut: &Shortcut) -> Option<String> {
    match overrides.get(shortcut.id).map(String::as_str) {
        Some("") => None,
        Some(keystroke) => canonical(keystroke).or_else(|| shortcut.default_keystroke()),
        None => shortcut.default_keystroke(),
    }
}

pub(in crate::ui) fn is_customized(overrides: &Overrides, shortcut: &Shortcut) -> bool {
    binding(overrides, shortcut) != shortcut.default_keystroke()
}

/// Another action that already answers to `keystroke`.
pub(in crate::ui) fn conflict(
    overrides: &Overrides,
    id: &str,
    keystroke: &str,
) -> Option<&'static Shortcut> {
    SHORTCUTS.iter().find(|shortcut| {
        shortcut.id != id && binding(overrides, shortcut).as_deref() == Some(keystroke)
    })
}

/// Binds `keystroke` to `id`, taking it from any action that had it; only differences from
/// the defaults are kept.
pub(in crate::ui) fn assign(overrides: &mut Overrides, id: &str, keystroke: Option<&str>) {
    let keystroke = keystroke.and_then(canonical);
    if let Some(other) = keystroke
        .as_deref()
        .and_then(|keystroke| conflict(overrides, id, keystroke))
    {
        store(overrides, other, None);
    }
    if let Some(shortcut) = find(id) {
        store(overrides, shortcut, keystroke);
    }
}

fn store(overrides: &mut Overrides, shortcut: &Shortcut, keystroke: Option<String>) {
    if keystroke == shortcut.default_keystroke() {
        overrides.remove(shortcut.id);
    } else {
        overrides.insert(shortcut.id.to_owned(), keystroke.unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assigning_a_taken_keystroke_moves_it_and_keeps_only_differences() {
        let mut overrides = Overrides::new();
        assert_eq!(
            conflict(&overrides, "new_tab", "cmd-n").map(|s| s.id),
            Some("new_chat")
        );
        assert!(conflict(&overrides, "new_chat", "cmd-n").is_none());
        assign(&mut overrides, "new_tab", Some("cmd-n"));
        assert_eq!(overrides.get("new_tab").map(String::as_str), Some("cmd-n"));
        assert_eq!(overrides.get("new_chat").map(String::as_str), Some(""));
        assign(&mut overrides, "new_chat", Some("cmd-n"));
        assert!(!overrides.contains_key("new_chat"));
        assert_eq!(overrides.get("new_tab").map(String::as_str), Some(""));
        assign(&mut overrides, "new_tab", Some("cmd-t"));
        assert!(overrides.is_empty());
        assign(&mut overrides, "zoom", Some("ctrl-cmd-z"));
        assert!(is_customized(&overrides, find("zoom").unwrap()));
    }
}
