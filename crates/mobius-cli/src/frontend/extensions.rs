//! Gateway-scoped extension catalog and lifecycle controls.

use std::io;

use mobius::{Error, Result};
use mobius_gateway::client::{GatewayEvents, GatewaySender, MAX_PENDING_FRAMES};
use mobius_gateway::wire::{
    ClientMessage, ExtensionHookRecord, ExtensionKind, ExtensionRecord, ReadyPayload, ServerFrame,
    ServerMessage,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, HighlightSpacing, List, ListState, Paragraph, Wrap};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use super::terminal::{INPUT_POLL, MAX_INPUT_BATCH, TerminalGuard, poll_event};
use super::terminal_text;
use super::theme::{Role, current};

const MAX_SOURCE_BYTES: usize = 4_096;

type ExtensionsTerminal = Terminal<CrosstermBackend<io::Stdout>>;

enum Mutation {
    Update,
    Uninstall,
    Trust(String),
    Untrust(String),
}

enum Confirmation {
    Uninstall {
        id: String,
        name: String,
    },
    Trust {
        id: String,
        name: String,
        digest: String,
        hooks: Vec<ExtensionHookRecord>,
        scroll: u16,
    },
}

enum Mode {
    Browse,
    Install(String),
    Confirm(Confirmation),
}

struct Pending {
    request_id: String,
    label: &'static str,
}

struct Notice {
    text: String,
    role: Role,
}

struct ExtensionsState {
    selected: usize,
    mode: Mode,
    pending: Option<Pending>,
    notice: Option<Notice>,
}

enum ScreenAction {
    None,
    Exit,
    Send {
        request_id: String,
        message: Box<ClientMessage>,
        label: &'static str,
    },
}

impl ExtensionsState {
    fn new(gateway: &ReadyPayload) -> Self {
        Self {
            selected: 0,
            mode: Mode::Browse,
            pending: None,
            notice: gateway.extensions.is_empty().then(|| Notice {
                text: "No extensions are installed. Press i to install one.".into(),
                role: Role::Muted,
            }),
        }
    }

    fn selected<'a>(&self, gateway: &'a ReadyPayload) -> Option<&'a ExtensionRecord> {
        gateway.extensions.get(self.selected)
    }

    fn clamp_selection(&mut self, gateway: &ReadyPayload) {
        self.selected = self
            .selected
            .min(gateway.extensions.len().saturating_sub(1));
    }

    fn move_selection(&mut self, gateway: &ReadyPayload, delta: isize) {
        if gateway.extensions.is_empty() {
            return;
        }
        self.selected =
            (self.selected as isize + delta).rem_euclid(gateway.extensions.len() as isize) as usize;
        self.notice = None;
    }

    fn handle_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> ScreenAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return ScreenAction::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'd'))
        {
            return ScreenAction::Exit;
        }
        if self.pending.is_some() {
            return if key.code == KeyCode::Char('q') {
                ScreenAction::Exit
            } else {
                ScreenAction::None
            };
        }
        if matches!(&self.mode, Mode::Browse) {
            self.handle_browse_key(key, gateway)
        } else if let Mode::Install(source) = &mut self.mode {
            let source = std::mem::take(source);
            self.handle_install_key(key, source)
        } else {
            self.handle_confirmation_key(key)
        }
    }

    fn handle_install_key(&mut self, key: KeyEvent, mut source: String) -> ScreenAction {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::Enter => {
                let source = source.trim().to_owned();
                if source.is_empty() {
                    self.fail("Enter an HTTPS Git or GitHub tree URL.");
                    return ScreenAction::None;
                }
                self.mode = Mode::Browse;
                install_action(source)
            }
            KeyCode::Backspace => {
                source.pop();
                self.mode = Mode::Install(source);
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let rejected = append_source(&mut source, &character.to_string());
                self.mode = Mode::Install(source);
                if rejected {
                    self.fail(format!(
                        "Source URL is limited to {MAX_SOURCE_BYTES} bytes."
                    ));
                } else {
                    self.notice = None;
                }
                ScreenAction::None
            }
            _ => {
                self.mode = Mode::Install(source);
                ScreenAction::None
            }
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent, gateway: &ReadyPayload) -> ScreenAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => ScreenAction::Exit,
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
                self.move_selection(gateway, -1);
                ScreenAction::None
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
                self.move_selection(gateway, 1);
                ScreenAction::None
            }
            KeyCode::Home => {
                self.selected = 0;
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::End if !gateway.extensions.is_empty() => {
                self.selected = gateway.extensions.len() - 1;
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::Char('i') => {
                self.mode = Mode::Install(String::new());
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::Char('u') => {
                let Some(extension) = self.selected(gateway) else {
                    self.fail("There is no extension to update.");
                    return ScreenAction::None;
                };
                id_action(Mutation::Update, extension.id.clone())
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                let Some(extension) = self.selected(gateway) else {
                    self.fail("There is no extension to uninstall.");
                    return ScreenAction::None;
                };
                self.mode = Mode::Confirm(Confirmation::Uninstall {
                    id: extension.id.clone(),
                    name: extension.name.clone(),
                });
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::Char('t') => {
                let Some(extension) = self.selected(gateway) else {
                    self.fail("There is no extension to trust.");
                    return ScreenAction::None;
                };
                if extension.hooks.is_empty() {
                    self.fail("The selected extension has no executable hooks.");
                } else if extension.hooks_trusted {
                    return id_action(
                        Mutation::Untrust(extension.digest.clone()),
                        extension.id.clone(),
                    );
                } else {
                    self.mode = Mode::Confirm(Confirmation::Trust {
                        id: extension.id.clone(),
                        name: extension.name.clone(),
                        digest: extension.digest.clone(),
                        hooks: extension.hooks.clone(),
                        scroll: 0,
                    });
                    self.notice = None;
                }
                ScreenAction::None
            }
            _ => ScreenAction::None,
        }
    }

    fn handle_confirmation_key(&mut self, key: KeyEvent) -> ScreenAction {
        match key.code {
            KeyCode::Up => {
                if let Mode::Confirm(Confirmation::Trust { scroll, .. }) = &mut self.mode {
                    *scroll = scroll.saturating_sub(1);
                }
                ScreenAction::None
            }
            KeyCode::PageUp => {
                if let Mode::Confirm(Confirmation::Trust { scroll, .. }) = &mut self.mode {
                    *scroll = scroll.saturating_sub(5);
                }
                ScreenAction::None
            }
            KeyCode::Down => {
                if let Mode::Confirm(Confirmation::Trust { scroll, .. }) = &mut self.mode {
                    *scroll = scroll.saturating_add(1);
                }
                ScreenAction::None
            }
            KeyCode::PageDown => {
                if let Mode::Confirm(Confirmation::Trust { scroll, .. }) = &mut self.mode {
                    *scroll = scroll.saturating_add(5);
                }
                ScreenAction::None
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.mode = Mode::Browse;
                self.notice = None;
                ScreenAction::None
            }
            KeyCode::Char('y' | 'Y') => {
                let Mode::Confirm(confirmation) = std::mem::replace(&mut self.mode, Mode::Browse)
                else {
                    unreachable!("confirmation handler requires a confirmation")
                };
                match confirmation {
                    Confirmation::Uninstall { id, .. } => id_action(Mutation::Uninstall, id),
                    Confirmation::Trust { id, digest, .. } => {
                        id_action(Mutation::Trust(digest), id)
                    }
                }
            }
            _ => ScreenAction::None,
        }
    }

    fn paste(&mut self, text: &str) {
        let Mode::Install(source) = &mut self.mode else {
            return;
        };
        if append_source(source, text.trim()) {
            self.fail(format!(
                "Source URL is limited to {MAX_SOURCE_BYTES} bytes."
            ));
        } else {
            self.notice = None;
        }
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice {
            text: message.into(),
            role: Role::Error,
        });
    }

    fn begin(&mut self, request_id: String, label: &'static str) {
        self.pending = Some(Pending { request_id, label });
        self.notice = None;
    }

    fn complete(&mut self) {
        if let Some(pending) = self.pending.take() {
            self.notice = Some(Notice {
                text: format!("{} complete.", pending.label),
                role: Role::Success,
            });
        }
    }
}

pub(super) async fn standalone(
    sender: GatewaySender,
    mut events: GatewayEvents,
    mut gateway: ReadyPayload,
) -> Result<()> {
    let mut guard = TerminalGuard::alternate()?;
    guard.set_mouse_capture(false)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    run(&mut terminal, &sender, &mut events, &mut gateway).await
}

pub(in crate::frontend) async fn run(
    terminal: &mut ExtensionsTerminal,
    sender: &GatewaySender,
    events: &mut GatewayEvents,
    gateway: &mut ReadyPayload,
) -> Result<()> {
    terminal.clear()?;
    let mut state = ExtensionsState::new(gateway);
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut deferred = Vec::new();
    let mut events_open = true;
    let mut dirty = true;

    let result = 'screen: loop {
        if dirty {
            terminal.draw(|frame| render(frame, &state, gateway))?;
            dirty = false;
        }
        tokio::select! {
            frame = events.next(), if events_open => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => break 'screen Err(gateway_error(error)),
                };
                match frame {
                    Some(frame) => match frame.message {
                        ServerMessage::Ready { payload } => {
                            *gateway = payload;
                            state.clamp_selection(gateway);
                        }
                        ServerMessage::GatewayConfigured { request_id, payload }
                            if state.pending.as_ref().is_some_and(|pending| {
                                pending.request_id == request_id
                            }) =>
                        {
                            *gateway = payload;
                            state.clamp_selection(gateway);
                            state.complete();
                        }
                        ServerMessage::Rejected { request_id, message, .. }
                            if state.pending.as_ref().is_some_and(|pending| {
                                pending.request_id == request_id
                            }) =>
                        {
                            state.pending = None;
                            state.fail(message);
                        }
                        ServerMessage::Sessions { sessions, .. } => gateway.sessions = sessions,
                        message if deferred.len() == MAX_PENDING_FRAMES => {
                            break 'screen Err(Error::Stopped(format!(
                                "gateway event backlog exceeds {MAX_PENDING_FRAMES} frames while managing extensions: {message:?}"
                            )));
                        }
                        message => deferred.push(ServerFrame::new(message)),
                    },
                    None => {
                        events_open = false;
                        state.pending = None;
                        state.fail("Gateway disconnected. Press q to close.");
                    }
                }
                dirty = true;
            }
            _ = tick.tick() => {
                for _ in 0..MAX_INPUT_BATCH {
                    let Some(event) = poll_event()? else {
                        break;
                    };
                    dirty = true;
                    let action = match event {
                        Event::Key(key) => state.handle_key(key, gateway),
                        Event::Paste(text) => {
                            state.paste(&text);
                            ScreenAction::None
                        }
                        Event::Resize(_, _)
                        | Event::FocusGained
                        | Event::FocusLost
                        | Event::Mouse(_) => ScreenAction::None,
                    };
                    match action {
                        ScreenAction::None => {}
                        ScreenAction::Exit => break 'screen Ok(()),
                        ScreenAction::Send { request_id, message, label } => {
                            match sender.send(*message).await {
                                Ok(()) => state.begin(request_id, label),
                                Err(error) => state.fail(error.to_string()),
                            }
                        }
                    }
                }
            }
        }
    };
    events.prepend(deferred).map_err(gateway_error)?;
    result
}

fn install_action(source: String) -> ScreenAction {
    let request_id = Uuid::new_v4().to_string();
    ScreenAction::Send {
        message: Box::new(ClientMessage::InstallExtension {
            request_id: request_id.clone(),
            source,
            reference: None,
            subdirectory: None,
        }),
        request_id,
        label: "Install",
    }
}

fn id_action(mutation: Mutation, id: String) -> ScreenAction {
    let request_id = Uuid::new_v4().to_string();
    let (message, label) = match mutation {
        Mutation::Update => (
            ClientMessage::UpdateExtension {
                request_id: request_id.clone(),
                id,
            },
            "Update",
        ),
        Mutation::Uninstall => (
            ClientMessage::UninstallExtension {
                request_id: request_id.clone(),
                id,
            },
            "Uninstall",
        ),
        Mutation::Trust(expected_digest) => (
            ClientMessage::TrustExtensionHooks {
                request_id: request_id.clone(),
                id,
                expected_digest,
            },
            "Trust",
        ),
        Mutation::Untrust(expected_digest) => (
            ClientMessage::RevokeExtensionHooksTrust {
                request_id: request_id.clone(),
                id,
                expected_digest,
            },
            "Untrust",
        ),
    };
    ScreenAction::Send {
        request_id,
        message: Box::new(message),
        label,
    }
}

fn append_source(source: &mut String, text: &str) -> bool {
    let mut rejected = false;
    for character in text.chars().filter(|character| !character.is_control()) {
        if source.len() + character.len_utf8() > MAX_SOURCE_BYTES {
            rejected = true;
            break;
        }
        source.push(character);
    }
    rejected
}

fn render(frame: &mut ratatui::Frame<'_>, state: &ExtensionsState, gateway: &ReadyPayload) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let area = content_area(frame.area());
    let prompt_height = match &state.mode {
        Mode::Install(_) => 3,
        Mode::Confirm(_) => 2,
        Mode::Browse => 1,
    };
    let [header, catalog, details, prompt, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Percentage(30),
        Constraint::Min(5),
        Constraint::Length(prompt_height),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("◉ ", theme.style(Role::AccentStrong)),
                Span::styled(
                    "MÖBIUS",
                    theme.style(Role::Accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" extensions", theme.style(Role::Muted)),
            ]),
            Line::styled(
                format!("  {}", terminal_text(&gateway.machine_name)),
                theme.style(Role::Muted),
            ),
        ]),
        header,
    );
    render_catalog(frame, catalog, state, gateway);
    render_details(frame, details, state, gateway);
    render_prompt(frame, prompt, state);
    let footer_text = matches!(state.mode, Mode::Browse)
        .then_some(
            "↑↓ select · i install · u update · x uninstall · t trust/untrust hooks · q close",
        )
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(footer_text).style(theme.style(Role::Muted)),
        footer,
    );
}

fn render_catalog(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &ExtensionsState,
    gateway: &ReadyPayload,
) {
    let theme = current();
    let block = Block::bordered()
        .title(format!(" Installed ({}) ", gateway.extensions.len()))
        .border_style(theme.style(Role::Border));
    if gateway.extensions.is_empty() {
        frame.render_widget(
            Paragraph::new(" No extensions installed")
                .style(theme.style(Role::Muted))
                .block(block),
            area,
        );
        return;
    }
    let rows = gateway.extensions.iter().map(|extension| {
        let version = extension
            .version
            .as_deref()
            .map_or(String::new(), |version| format!(" · {version}"));
        let hook_status = if extension.hooks.is_empty() {
            "no hooks"
        } else if extension.hooks_trusted {
            "hooks trusted"
        } else {
            "hooks untrusted"
        };
        Line::from(vec![
            Span::styled(
                terminal_text(&extension.name),
                theme.style(Role::Text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {}{} · {hook_status}",
                    extension_kind(extension.kind),
                    terminal_text(&version)
                ),
                theme.style(Role::Muted),
            ),
        ])
    });
    let mut list_state = ListState::default().with_selected(Some(state.selected));
    frame.render_stateful_widget(
        List::new(rows)
            .block(block)
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::default().add_modifier(Modifier::BOLD)),
        area,
        &mut list_state,
    );
}

fn render_details(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &ExtensionsState,
    gateway: &ReadyPayload,
) {
    let theme = current();
    let block = Block::bordered().border_style(theme.style(Role::Border));
    let (lines, scroll) = match &state.mode {
        Mode::Confirm(Confirmation::Trust {
            name,
            digest,
            hooks,
            scroll,
            ..
        }) => (trust_review(name, digest, hooks), *scroll),
        _ => (
            state.selected(gateway).map_or_else(
                || vec![Line::styled("Nothing selected", theme.style(Role::Muted))],
                extension_details,
            ),
            0,
        ),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0))
            .block(block),
        area,
    );
}

fn extension_details(extension: &ExtensionRecord) -> Vec<Line<'static>> {
    let theme = current();
    let mut lines = vec![
        Line::styled(
            terminal_text(&extension.name),
            theme.style(Role::AccentStrong).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            terminal_text(&extension.description),
            theme.style(Role::Text),
        ),
        Line::styled(
            format!("source: {}", terminal_text(&extension.source)),
            theme.style(Role::Muted),
        ),
    ];
    if let Some(reference) = &extension.reference {
        lines.push(Line::styled(
            format!("reference: {}", terminal_text(reference)),
            theme.style(Role::Muted),
        ));
    }
    if let Some(subdirectory) = &extension.subdirectory {
        lines.push(Line::styled(
            format!("path: {}", terminal_text(subdirectory)),
            theme.style(Role::Muted),
        ));
    }
    lines.extend([
        Line::styled(
            format!(
                "revision: {} · digest: {}",
                terminal_text(&extension.resolved_revision),
                terminal_text(&extension.digest)
            ),
            theme.style(Role::Muted),
        ),
        Line::styled(
            if extension.skills.is_empty() {
                "skills: none".into()
            } else {
                format!("skills: {}", terminal_text(&extension.skills.join(", ")))
            },
            theme.style(Role::Info),
        ),
    ]);
    if extension.hooks.is_empty() {
        lines.push(Line::styled("hooks: none", theme.style(Role::Muted)));
    } else {
        lines.push(Line::styled(
            if extension.hooks_trusted {
                "hooks: trusted for this digest"
            } else {
                "hooks: untrusted — review every command before pressing t"
            },
            theme.style(if extension.hooks_trusted {
                Role::Success
            } else {
                Role::Warning
            }),
        ));
        lines.extend(extension.hooks.iter().map(hook_line));
    }
    lines
}

fn trust_review(name: &str, digest: &str, hooks: &[ExtensionHookRecord]) -> Vec<Line<'static>> {
    let theme = current();
    let mut lines = vec![
        Line::styled(
            format!("Trust executable hooks from {}?", terminal_text(name)),
            theme.style(Role::Warning).add_modifier(Modifier::BOLD),
        ),
        Line::styled(
            format!("digest: {}", terminal_text(digest)),
            theme.style(Role::Muted),
        ),
    ];
    lines.extend(hooks.iter().map(hook_line));
    lines
}

fn hook_line(hook: &ExtensionHookRecord) -> Line<'static> {
    let matcher = hook.matcher.as_deref().map_or(String::new(), |matcher| {
        format!(" [{}]", manifest_text(matcher))
    });
    Line::styled(
        format!(
            "{}{} · {} · {}s",
            manifest_text(&hook.event),
            matcher,
            manifest_text(&hook.command),
            hook.timeout_seconds
        ),
        current().style(Role::Code),
    )
}

fn manifest_text(value: &str) -> String {
    terminal_text(&value.escape_debug().to_string())
}

fn render_prompt(frame: &mut ratatui::Frame<'_>, area: Rect, state: &ExtensionsState) {
    let theme = current();
    let (text, role) = if let Some(pending) = &state.pending {
        (format!("◉ {} in progress…", pending.label), Role::Info)
    } else {
        match &state.mode {
            Mode::Install(source) => (
                format!(
                    "Install from an HTTPS Git URL or GitHub tree URL\nsource: {}▏ · Enter install · Esc cancel",
                    terminal_text(source)
                ),
                Role::Text,
            ),
            Mode::Confirm(Confirmation::Uninstall { name, .. }) => (
                format!(
                    "Uninstall {}?\nWorkspace .mobius/extensions data is retained. y/N",
                    terminal_text(name)
                ),
                Role::Warning,
            ),
            Mode::Confirm(Confirmation::Trust { name, .. }) => (
                format!(
                    "Trust the hooks shown above for {}? y/N\n↑↓/PgUp/PgDn review all commands",
                    terminal_text(name)
                ),
                Role::Warning,
            ),
            Mode::Browse => state.notice.as_ref().map_or_else(
                || (String::new(), Role::Muted),
                |notice| (terminal_text(&notice.text), notice.role),
            ),
        }
    };
    frame.render_widget(Paragraph::new(text).style(theme.style(role)), area);
}

fn content_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(100);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y.saturating_add(1),
        width,
        area.height.saturating_sub(2),
    )
}

const fn extension_kind(kind: ExtensionKind) -> &'static str {
    match kind {
        ExtensionKind::Skill => "skill",
        ExtensionKind::Plugin => "plugin",
    }
}

fn gateway_error(error: mobius_gateway::Error) -> Error {
    Error::Stopped(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extension() -> ExtensionRecord {
        ExtensionRecord {
            id: "ponytail".into(),
            capability: "capability-a".into(),
            kind: ExtensionKind::Plugin,
            name: "Ponytail".into(),
            description: "Minimal solutions".into(),
            version: Some("4.9.0".into()),
            source: "https://github.com/example/ponytail".into(),
            reference: Some("main".into()),
            subdirectory: None,
            resolved_revision: "abc123".into(),
            digest: "sha256:trusted-snapshot".into(),
            skills: vec!["ponytail".into()],
            hooks: vec![ExtensionHookRecord {
                event: "pre_tool".into(),
                matcher: Some("shell".into()),
                command: "review-hook --safe".into(),
                timeout_seconds: 10,
            }],
            hooks_trusted: false,
        }
    }

    #[test]
    fn install_uses_the_gateway_scoped_wire_operation() {
        let ScreenAction::Send {
            request_id,
            message,
            label,
        } = install_action("https://github.com/example/ponytail/tree/main/plugin".into())
        else {
            panic!("install request")
        };
        let ClientMessage::InstallExtension {
            request_id: wire_id,
            source,
            reference,
            subdirectory,
        } = *message
        else {
            panic!("install wire operation")
        };

        assert_eq!(request_id, wire_id);
        assert_eq!(
            source,
            "https://github.com/example/ponytail/tree/main/plugin"
        );
        assert_eq!((reference, subdirectory), (None, None));
        assert_eq!(label, "Install");
    }

    #[test]
    fn lifecycle_requests_bind_identity_and_hook_trust_to_the_digest() {
        let record = extension();
        let actions = [
            id_action(Mutation::Update, record.id.clone()),
            id_action(Mutation::Uninstall, record.id.clone()),
            id_action(Mutation::Trust(record.digest.clone()), record.id.clone()),
            id_action(Mutation::Untrust(record.digest.clone()), record.id.clone()),
        ];

        let messages = actions.map(|action| {
            let ScreenAction::Send { message, .. } = action else {
                panic!("lifecycle wire operation")
            };
            message
        });
        assert!(matches!(
            messages[0].as_ref(),
            ClientMessage::UpdateExtension { id, .. } if id == &record.id
        ));
        assert!(matches!(
            messages[1].as_ref(),
            ClientMessage::UninstallExtension { id, .. } if id == &record.id
        ));
        assert!(matches!(
            messages[2].as_ref(),
            ClientMessage::TrustExtensionHooks { id, expected_digest, .. }
                if id == &record.id && expected_digest == &record.digest
        ));
        assert!(matches!(
            messages[3].as_ref(),
            ClientMessage::RevokeExtensionHooksTrust { id, expected_digest, .. }
                if id == &record.id && expected_digest == &record.digest
        ));
    }

    #[test]
    fn source_input_filters_controls_and_stops_at_the_wire_limit() {
        let mut source = String::new();

        assert!(!append_source(&mut source, "https://example.com/repo\n"));
        assert!(append_source(&mut source, &"a".repeat(MAX_SOURCE_BYTES)));

        assert!(!source.contains('\n'));
        assert_eq!(source.len(), MAX_SOURCE_BYTES);
    }

    #[test]
    fn trust_review_shows_every_executable_command() {
        let mut record = extension();
        record.hooks.push(ExtensionHookRecord {
            event: "Stop".into(),
            matcher: None,
            command: "first\nsecond\targument".into(),
            timeout_seconds: 5,
        });
        let text = trust_review(&record.name, &record.digest, &record.hooks)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(text.contains(&record.digest));
        assert!(text.contains("pre_tool [shell] · review-hook --safe · 10s"));
        assert!(text.contains(r"first\nsecond\targument"));
    }
}
