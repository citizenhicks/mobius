//! Terminal tabs: a login shell in the chat's folder, hosted by a native SwiftTerm view.

use super::native::NativeView;
use super::*;
use crate::host::platform::TerminalLaunch;
use std::path::{Path, PathBuf};

const GLYPH: &str = "hi.terminalWindow";

#[derive(Clone, Debug, PartialEq)]
pub(in crate::ui) struct TerminalDoc {
    shell: String,
    folder: PathBuf,
    /// The shell's exit status once it ended: its code, or `None` when unknown.
    exited: Option<Option<i32>>,
}

/// The user's login shell, as the terminal launches it.
fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| shell.starts_with('/'))
        .unwrap_or_else(|| "/bin/zsh".into())
}

/// "zsh · project": the shell's name and the folder it started in.
fn terminal_title(shell: &str, folder: &Path) -> String {
    let name = |path: &Path| {
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
    };
    let shell = name(Path::new(shell)).unwrap_or_else(|| "shell".into());
    match name(folder) {
        Some(folder) => format!("{shell} · {folder}"),
        None => shell,
    }
}

fn exit_label(status: Option<i32>) -> String {
    match status {
        Some(0) | None => "Process exited".into(),
        Some(code) => format!("Process exited with status {code}"),
    }
}

fn components(color: Hsla) -> [f32; 4] {
    let Rgba { r, g, b, a } = color.into();
    [r, g, b, a]
}

impl Workspace {
    /// The open chat's folder on this Mac, or why a terminal cannot start there.
    pub(in crate::ui) fn terminal_availability(&self) -> Result<PathBuf, &'static str> {
        let folder = if self.active_session_ready() {
            self.client
                .state
                .session
                .as_ref()
                .and_then(|session| session.workspace.as_ref())
                .map(|workspace| workspace.path.as_path())
        } else if self.draft_selected() {
            Some(self.draft_workspace.as_path())
        } else {
            None
        };
        crate::gateway::local::terminal_folder(self.client.local_gateway, folder)
    }

    /// Opens a new terminal tab in the chat's folder; bind shortcuts to this.
    pub(in crate::ui) fn open_terminal(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        match self.terminal_availability() {
            Ok(folder) => self.open_terminal_in(folder, cx),
            Err(reason) => {
                self.report(reason);
                cx.notify();
            }
        }
    }

    pub(in crate::ui) fn open_terminal_in(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        let shell = login_shell();
        self.open_content(
            ContentTab {
                id: format!("terminal:{}", crate::gateway::request_id()).into(),
                title: terminal_title(&shell, &folder).into(),
                content: TabContent::Terminal(TerminalDoc {
                    shell,
                    folder,
                    exited: None,
                }),
            },
            cx,
        );
    }

    /// Records a tab's shell exit so its toolbar offers Restart.
    pub(in crate::ui) fn terminal_exited(&mut self, tab_id: &str, status: Option<i32>) {
        if let Some(tab) = self.content.tabs.get_mut(tab_id)
            && let TabContent::Terminal(doc) = &mut tab.content
        {
            doc.exited = Some(status);
        }
    }

    /// Starts a fresh shell: the native view closes and the next paint creates another.
    fn restart_terminal(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if let Some(tab) = self.content.tabs.get_mut(tab_id)
            && let TabContent::Terminal(doc) = &mut tab.content
        {
            doc.exited = None;
            self.native.close_panel(tab_id);
            cx.notify();
        }
    }

    pub(super) fn terminal_document(
        &self,
        tab_id: &str,
        doc: &TerminalDoc,
        cx: &Context<Self>,
    ) -> AnyElement {
        let p = self.palette;
        let folder = doc.folder.to_string_lossy().into_owned();
        let bar = toolbar()
            .child(path_label(&folder, GLYPH, cx))
            .child(spacer())
            .when_some(doc.exited, |bar, status| {
                let id = tab_id.to_owned();
                bar.child(status_label(p.muted, exit_label(status), cx))
                    .child(button("terminal-restart", "Restart").on_click(
                        cx.listener(move |this, _, _, cx| this.restart_terminal(&id, cx)),
                    ))
            });
        let launch = TerminalLaunch {
            shell: doc.shell.clone(), // rules-allow: every render hands the native terminal its own launch description
            folder,
            background: components(p.card.alpha(1.)),
            foreground: components(p.text),
            caret: components(p.text),
            selection: components(p.accent_soft),
            font_set: FontSet::current(),
        };
        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .pl(space::S)
            .py(space::S)
            .child(Self::native_content(
                tab_id,
                NativeView::Terminal(launch),
                cx,
            ));
        document(bar, body, cx)
    }
}
