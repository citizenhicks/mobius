//! Focused saved-gateway settings.

use std::io;

use mobius::{Error, Result};
use mobius_gateway::client::{Endpoint, GatewayClient};
use mobius_gateway::wire::ClientKind;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use tokio::time::MissedTickBehavior;

use super::terminal::{INPUT_POLL, MAX_INPUT_BATCH, poll_event};
use super::terminal_text;
use super::theme::{Role, current};
use crate::gateway_accounts::{GatewayAccounts, environment_override_message};

const MAX_ENDPOINT_BYTES: usize = 4 * 1024;
const MAX_PAIRING_CODE_BYTES: usize = 512;

type GatewayTerminal = Terminal<CrosstermBackend<io::Stdout>>;

pub(crate) async fn run(terminal: &mut GatewayTerminal, connected: &str) -> Result<bool> {
    let accounts = GatewayAccounts::load().map_err(gateway_error)?;
    let mut state = State::new(accounts, connected);
    let mut tick = tokio::time::interval(INPUT_POLL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    terminal.clear()?;

    loop {
        draw(terminal, &state)?;
        tick.tick().await;
        for _ in 0..MAX_INPUT_BATCH {
            let Some(event) = poll_event()? else {
                break;
            };
            let action = match event {
                Event::Key(key) => state.handle_key(key),
                Event::Paste(value) => {
                    state.insert(&value);
                    Ok(Action::None)
                }
                Event::Resize(_, _) => Ok(Action::None),
                Event::FocusGained | Event::FocusLost | Event::Mouse(_) => Ok(Action::None),
            };
            match action {
                Ok(Action::None) => {}
                Ok(Action::Cancel) => return Ok(false),
                Ok(Action::Reconnect) => return Ok(true),
                Ok(Action::Pair) => {
                    let (endpoint, code) = match state.pairing_request() {
                        Ok(request) => request,
                        Err(error) => {
                            state.error = Some(error.to_string());
                            continue;
                        }
                    };
                    if let Err(error) = state.accounts.prepare() {
                        state.error = Some(error.to_string());
                        continue;
                    }
                    state.pairing = true;
                    state.error = None;
                    draw(terminal, &state)?;
                    let paired =
                        GatewayClient::pair(&endpoint, code, "mobius-cli", ClientKind::Cli).await;
                    state.pairing = false;
                    match paired {
                        Ok((_client, paired)) => {
                            let mut accounts = state.accounts.clone();
                            let saved = accounts
                                .add(&endpoint, paired.token)
                                .and_then(|()| accounts.save());
                            match saved {
                                Ok(()) => return Ok(true),
                                Err(error) => {
                                    state.error = Some(format!(
                                        "paired, but could not save the gateway: {error}"
                                    ));
                                }
                            }
                        }
                        Err(error) => state.error = Some(error.to_string()),
                    }
                }
                Err(error) => state.error = Some(error.to_string()),
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Accounts,
    Add,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Endpoint,
    PairingCode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    None,
    Cancel,
    Reconnect,
    Pair,
}

struct State {
    accounts: GatewayAccounts,
    connected: String,
    page: Page,
    selection: usize,
    field: Field,
    endpoint: String,
    pairing_code: String,
    pairing: bool,
    override_message: Option<&'static str>,
    error: Option<String>,
}

impl State {
    fn new(accounts: GatewayAccounts, connected: &str) -> Self {
        let selection = accounts
            .endpoints()
            .position(|endpoint| endpoint == connected)
            .or_else(|| {
                accounts
                    .selected()
                    .and_then(|selected| accounts.endpoints().position(|item| item == selected))
            })
            .unwrap_or_default();
        Self {
            accounts,
            connected: connected.into(),
            page: Page::Accounts,
            selection,
            field: Field::Endpoint,
            endpoint: String::new(),
            pairing_code: String::new(),
            pairing: false,
            override_message: environment_override_message(),
            error: None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> mobius_gateway::Result<Action> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Ok(Action::None);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(Action::Cancel);
        }
        self.error = None;
        match self.page {
            Page::Accounts => self.handle_accounts_key(key),
            Page::Add => self.handle_add_key(key),
        }
    }

    fn handle_accounts_key(&mut self, key: KeyEvent) -> mobius_gateway::Result<Action> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Ok(Action::Cancel),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Ok(Action::None)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Ok(Action::None)
            }
            KeyCode::Char('a') => self.open_add(),
            KeyCode::Char('d') | KeyCode::Delete => self.forget_selected(),
            KeyCode::Enter | KeyCode::Char('r') => self.reconnect_selected(),
            KeyCode::Char(value @ '1'..='9') => {
                let selection = value as usize - '1' as usize;
                if selection < self.account_row_count() {
                    self.selection = selection;
                }
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent) -> mobius_gateway::Result<Action> {
        match key.code {
            KeyCode::Esc => {
                self.page = Page::Accounts;
                self.pairing_code.clear();
                Ok(Action::None)
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                self.field = match self.field {
                    Field::Endpoint => Field::PairingCode,
                    Field::PairingCode => Field::Endpoint,
                };
                Ok(Action::None)
            }
            KeyCode::Backspace => {
                self.selected_input().pop();
                Ok(Action::None)
            }
            KeyCode::Enter if self.field == Field::Endpoint => {
                self.endpoint.trim().parse::<Endpoint>()?;
                self.field = Field::PairingCode;
                Ok(Action::None)
            }
            KeyCode::Enter => Ok(Action::Pair),
            KeyCode::Char(value)
                if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.insert_char(value);
                Ok(Action::None)
            }
            _ => Ok(Action::None),
        }
    }

    fn reconnect_selected(&mut self) -> mobius_gateway::Result<Action> {
        self.require_saved_management()?;
        let Some(endpoint) = self
            .accounts
            .endpoints()
            .nth(self.selection)
            .map(str::to_owned)
        else {
            return self.open_add();
        };
        let mut accounts = self.accounts.clone();
        accounts.select(&endpoint)?;
        accounts.save()?;
        self.accounts = accounts;
        Ok(Action::Reconnect)
    }

    fn forget_selected(&mut self) -> mobius_gateway::Result<Action> {
        self.require_saved_management()?;
        let Some(endpoint) = self
            .accounts
            .endpoints()
            .nth(self.selection)
            .map(str::to_owned)
        else {
            return Ok(Action::None);
        };
        let mut accounts = self.accounts.clone();
        accounts.forget(&endpoint);
        accounts.save()?;
        self.accounts = accounts;
        self.selection = self
            .selection
            .min(self.account_row_count().saturating_sub(1));
        Ok(Action::None)
    }

    fn open_add(&mut self) -> mobius_gateway::Result<Action> {
        self.require_saved_management()?;
        self.page = Page::Add;
        self.field = Field::Endpoint;
        self.endpoint.clear();
        self.pairing_code.clear();
        Ok(Action::None)
    }

    fn require_saved_management(&self) -> mobius_gateway::Result<()> {
        if let Some(message) = self.override_message {
            return Err(mobius_gateway::Error::Config(message.into()));
        }
        Ok(())
    }

    fn move_selection(&mut self, direction: isize) {
        let rows = self.account_row_count();
        if rows == 0 {
            return;
        }
        self.selection = (self.selection as isize + direction).rem_euclid(rows as isize) as usize;
    }

    fn account_row_count(&self) -> usize {
        self.accounts.endpoints().len() + usize::from(self.override_message.is_none())
    }

    fn selected_input(&mut self) -> &mut String {
        match self.field {
            Field::Endpoint => &mut self.endpoint,
            Field::PairingCode => &mut self.pairing_code,
        }
    }

    fn insert_char(&mut self, value: char) {
        let limit = self.input_limit();
        let input = self.selected_input();
        if input.len() + value.len_utf8() <= limit {
            input.push(value);
        } else {
            self.error = Some(format!("input is limited to {limit} bytes"));
        }
    }

    fn insert(&mut self, value: &str) {
        if self.page != Page::Add {
            return;
        }
        let limit = self.input_limit();
        let input = self.selected_input();
        if input.len() + value.len() <= limit {
            input.push_str(value);
        } else {
            self.error = Some(format!("input is limited to {limit} bytes"));
        }
    }

    const fn input_limit(&self) -> usize {
        match self.field {
            Field::Endpoint => MAX_ENDPOINT_BYTES,
            Field::PairingCode => MAX_PAIRING_CODE_BYTES,
        }
    }

    fn pairing_request(&self) -> mobius_gateway::Result<(Endpoint, String)> {
        let endpoint = self.endpoint.trim().parse()?;
        let code = self.pairing_code.trim().to_owned();
        if code.is_empty() {
            return Err(mobius_gateway::Error::Config(
                "enter the one-time code".into(),
            ));
        }
        Ok((endpoint, code))
    }
}

fn draw(terminal: &mut GatewayTerminal, state: &State) -> Result<()> {
    terminal.draw(|frame| render(frame, state))?;
    Ok(())
}

fn render(frame: &mut ratatui::Frame<'_>, state: &State) {
    let theme = current();
    frame.render_widget(
        Block::default().style(theme.style(Role::Canvas)),
        frame.area(),
    );
    let area = content_area(frame.area());
    let page = match state.page {
        Page::Accounts => 1,
        Page::Add => 2,
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("◉ ", theme.style(Role::AccentStrong)),
            Span::styled(
                "MÖBIUS",
                theme.style(Role::Accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" gateway settings", theme.style(Role::Muted)),
        ]),
        Line::styled(format!("Page {page} of 2"), theme.style(Role::Muted)),
        Line::from(""),
    ];
    match state.page {
        Page::Accounts => render_accounts(&mut lines, state),
        Page::Add => render_add(&mut lines, state),
    }
    if let Some(error) = &state.error {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("  {}", terminal_text(error)),
            theme.style(Role::Error),
        ));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.style(Role::Canvas))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_accounts(lines: &mut Vec<Line<'static>>, state: &State) {
    let theme = current();
    lines.push(Line::styled(
        "  Current gateway",
        theme.style(Role::Text).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::from(vec![
        Span::styled("  ● ", theme.style(Role::Success)),
        Span::styled(terminal_text(&state.connected), theme.style(Role::Text)),
        Span::styled(" · connected", theme.style(Role::Muted)),
    ]));
    if let Some(message) = state.override_message {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            format!("  {}", terminal_text(message)),
            theme.style(Role::Warning),
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        "  Saved gateways",
        theme.style(Role::Text).add_modifier(Modifier::BOLD),
    ));
    if state.accounts.endpoints().len() == 0 {
        lines.push(Line::styled(
            "  None saved · möbius uses the automatic local gateway by default.",
            theme.style(Role::Muted),
        ));
    }
    for (index, endpoint) in state.accounts.endpoints().enumerate() {
        let status = match (
            endpoint == state.connected,
            state.accounts.selected() == Some(endpoint),
        ) {
            (true, true) => "connected · selected",
            (true, false) => "connected",
            (false, true) => "selected",
            (false, false) => "saved",
        };
        choice(lines, index, endpoint, status, index == state.selection);
    }
    if state.override_message.is_none() {
        let index = state.accounts.endpoints().len();
        choice(
            lines,
            index,
            "Add gateway…",
            "Pair with an endpoint using a one-time code",
            index == state.selection,
        );
    }
    lines.push(Line::from(""));
    lines.push(Line::styled(
        if state.override_message.is_some() {
            "  ↑↓/j k inspect · esc/q return"
        } else {
            "  enter/r reconnect · a add · d delete · esc/q return"
        },
        theme.style(Role::Muted),
    ));
}

fn render_add(lines: &mut Vec<Line<'static>>, state: &State) {
    let theme = current();
    lines.push(Line::styled(
        "  Add gateway",
        theme.style(Role::Text).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        "  Use tcp:// only for loopback; remote gateways require tls://.",
        theme.style(Role::Muted),
    ));
    lines.push(Line::from(""));
    field(
        lines,
        "Endpoint",
        &terminal_text(&state.endpoint),
        state.field == Field::Endpoint,
    );
    field(
        lines,
        "One-time code",
        &masked(&state.pairing_code),
        state.field == Field::PairingCode,
    );
    lines.push(Line::from(""));
    lines.push(Line::styled(
        if state.pairing {
            "  Pairing securely…"
        } else {
            "  tab/↑↓ switch field · enter continue/pair · esc back · ctrl-c return"
        },
        theme.style(if state.pairing {
            Role::Info
        } else {
            Role::Muted
        }),
    ));
}

fn choice(
    lines: &mut Vec<Line<'static>>,
    index: usize,
    label: &str,
    description: &str,
    selected: bool,
) {
    let theme = current();
    let role = if selected {
        Role::Selection
    } else {
        Role::Text
    };
    lines.push(Line::styled(
        format!(
            "{} {}. {}",
            if selected { "›" } else { " " },
            index + 1,
            terminal_text(label)
        ),
        theme.style(role),
    ));
    lines.push(Line::styled(
        format!("     {}", terminal_text(description)),
        theme.style(if selected {
            Role::Selection
        } else {
            Role::Muted
        }),
    ));
}

fn field(lines: &mut Vec<Line<'static>>, label: &str, value: &str, selected: bool) {
    let theme = current();
    lines.push(Line::styled(
        format!("{} {label}", if selected { "›" } else { " " }),
        theme.style(if selected {
            Role::Selection
        } else {
            Role::Text
        }),
    ));
    lines.push(Line::styled(
        format!("    {value}{}", if selected { "▏" } else { "" }),
        theme.style(if selected { Role::Info } else { Role::Muted }),
    ));
}

fn masked(value: &str) -> String {
    let count = value.chars().count();
    let mut masked = "•".repeat(count.min(32));
    if count > 32 {
        masked.push('…');
    }
    masked
}

fn content_area(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(82);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y.saturating_add(1),
        width,
        area.height.saturating_sub(2),
    )
}

fn gateway_error(error: mobius_gateway::Error) -> Error {
    Error::Stopped(error.to_string())
}
