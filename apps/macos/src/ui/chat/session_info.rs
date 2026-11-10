//! The Session pill and the session card it pins over the chat (ComposerView.swift
//! SessionStatsBadge, ChatView.swift chat info).

use super::controls::{file_detail, file_glyph};
use super::*;
use crate::numeric::{Saturating, ToFloat};
use mobius::SessionId;
use mobius::protocol::{ContextBreakdown, ContextPartKind, TokenUsageInfo};
use mobius_gateway::wire::{ClientMessage, SessionReadyPayload};
use std::time::Instant;

/// The `action_requests` key of the chat's memory switch while its request is pending.
const MEMORY_REQUEST: &str = "session-memory";

/// Context fill, elapsed run time and the run totals behind the Session pill.
struct SessionMetrics {
    fraction: Option<f32>,
    elapsed_ms: u64,
    context: Option<i64>,
    limit: Option<i64>,
}

impl SessionMetrics {
    /// "42% · 35 min", or only the time while the context size is unknown.
    fn summary(&self) -> String {
        let elapsed = compact_duration(self.elapsed_ms);
        match self.fraction {
            Some(fraction) => format!("{} · {elapsed}", percent(fraction.into())),
            None => elapsed,
        }
    }
}

pub(in crate::ui) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

fn session_metrics(
    session: &SessionReadyPayload,
    usage: Option<&TokenUsageInfo>,
    now_ms: i64,
) -> SessionMetrics {
    let complete = &session.run_stats.completed;
    let active = session.run_stats.active.as_ref();
    let last = usage.map(|info| &info.last_token_usage);
    let context = last.map(|usage| {
        usage
            .total_tokens
            .max(usage.input_tokens.saturating_add(usage.output_tokens))
            .max(0)
    });
    let limit = session.context_limit_tokens.filter(|limit| *limit > 0);
    let fraction = context
        .zip(limit)
        .map(|(used, limit)| (used.to_f32() / limit.to_f32()).clamp(0., 1.));
    let live = active.map_or(0, |run| {
        run.elapsed_ms.max(
            now_ms
                .saturating_sub(run.started_at_ms)
                .max(0)
                .cast_unsigned(),
        )
    });
    let elapsed_ms = complete.elapsed_ms.saturating_add(live);
    SessionMetrics {
        fraction,
        elapsed_ms,
        context,
        limit,
    }
}

fn cache_hit(usage: &mobius::protocol::TokenUsage) -> Option<String> {
    (usage.input_tokens > 0).then(|| {
        format!(
            "{:.1}%",
            usage.cached_input_tokens.to_f64() / usage.input_tokens.to_f64() * 100.
        )
    })
}

impl Workspace {
    /// The Session pill: a ring filled with the context used, "12% · 3 min", or only the
    /// elapsed time beside a clock while the gateway has not reported usage yet.
    pub(super) fn context_pill(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let session = self.client.state.session.as_ref()?;
        let metrics = session_metrics(session, self.client.state.token_usage.as_deref(), now_ms());
        let summary = SharedString::from(metrics.summary());
        let id = session.session.session_id.share();
        let item = match metrics.fraction {
            Some(fraction) => pill("chat-context").progress(fraction),
            None => pill("chat-context").glyph("hi.clock"),
        };
        Some(
            item.label(SharedString::clone(&summary))
                .button(cx)
                .accessibility_label(format!("Context: {summary}"))
                .tooltip("Context and session")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_chat_detail(ChatDetail::Context(id.share()), window, cx);
                }))
                .into_any_element(),
        )
    }

    /// Shows or hides the session card of `session`.
    pub(in crate::ui) fn toggle_session_card(
        &mut self,
        session: &SessionId,
        cx: &mut Context<Self>,
    ) {
        let open = self.chat_ui.session_card.as_ref() == Some(session);
        self.set_session_card(session, !open, cx);
    }

    /// Closes the card so a chat beside documents keeps its width.
    pub(in crate::ui) fn close_session_card(&mut self) {
        if let Some(session) = self.chat_ui.session_card.take() {
            // Render clears the slide when reduce motion is on.
            self.chat_ui
                .session_card_motion
                .set(&session, false, false, Instant::now());
        }
    }

    fn set_session_card(&mut self, session: &SessionId, open: bool, cx: &mut Context<Self>) {
        self.chat_ui
            .session_card_motion
            .set(session, open, cx.reduce_motion(), Instant::now());
        self.chat_ui.session_card = open.then(|| session.share());
        cx.notify();
    }

    /// The header button showing or hiding the open chat's session card.
    pub(in crate::ui) fn session_card_button(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let session = self
            .client
            .state
            .selected_session_id
            .as_ref()
            .map(SessionId::share)?;
        let shown = self.chat_ui.session_card.as_ref() == Some(&session);
        let label = if shown {
            "Hide session info"
        } else {
            "Show session info"
        };
        let button = toolbar_button("session-card-toggle", label, "hi.filterHorizontal", cx)
            .toggle(shown, cx)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_session_card(&session, cx)));
        #[cfg(feature = "snapshot")]
        let button = div()
            .relative()
            .child(button)
            .child(snapshot::probe("motion:session-toggle".into()));
        Some(button.into_any_element())
    }

    /// Where and what runs the chat: gateway, workspace, Bot and model.
    fn session_identity(&self, session: &SessionReadyPayload) -> Vec<(&'static str, String)> {
        let mut rows = vec![("Gateway", self.gateway_name())];
        if let Some(workspace) = &session.workspace {
            let path = workspace.path.display().to_string();
            rows.push(("Workspace", super::controls::folder_name(&path)));
        }
        if let Some(bot) = self.composer_bot() {
            rows.push(("Bot", bot.name.to_string()));
        }
        let owner = ModelTarget::Bot(Arc::clone(&session.session.context.owner_id));
        if let Some(levels) = self.target_levels(&owner) {
            let (name, _) = self.model_identity(levels.current);
            let effort = self.effort_label(levels.current);
            rows.push(("Model", format!("{name} · {effort}")));
        }
        rows
    }

    /// The run totals: "4 runs", "92 tool calls", "7M tokens", "74% cache hit".
    fn session_totals(&self, session: &SessionReadyPayload) -> Vec<String> {
        let complete = &session.run_stats.completed;
        let active = session.run_stats.active.as_ref();
        let runs = complete.run_count + u64::from(active.is_some());
        let tools = complete.tool_calls + active.map_or(0, |run| run.tool_calls);
        let tokens = complete.usage.total_tokens + active.map_or(0, |run| run.usage.total_tokens);
        let mut totals = vec![
            counted(runs, "run", "runs"),
            counted(tools, "tool call", "tool calls"),
            format!("{} tokens", compact_count(tokens.max(0))),
        ];
        totals.extend(session.contributions.iter().filter_map(|contribution| {
            let label = contribution.count_label.as_ref()?;
            Some(counted(contribution.count?, &label.one, &label.other))
        }));
        totals.extend(
            self.client
                .state
                .token_usage
                .as_ref()
                .and_then(|usage| cache_hit(&usage.last_token_usage))
                .map(|hit| format!("{hit} cache hit")),
        );
        totals
    }

    /// The open chat's card width while it slides, keeping a closing card mounted until
    /// it finishes; idle cards request no frames.
    fn track_session_card_motion(&mut self, window: &Window, cx: &App) -> Option<Pixels> {
        let now = Instant::now();
        let motion = &mut self.chat_ui.session_card_motion;
        if cx.reduce_motion() {
            motion.clear();
        }
        motion.settle(now);
        let id = self.client.state.selected_session_id.as_deref()?;
        motion.contains(id).then(|| {
            window.request_animation_frame();
            let open = self.chat_ui.session_card.as_deref() == Some(id);
            (width::POPOVER + space::S) * motion.shown(id, open, now)
        })
    }

    /// Opens the Bot's file `id` from the chat's current listing, as the New tab does.
    fn open_chat_output(&mut self, session: SessionId, id: &str, cx: &mut Context<Self>) {
        let Some(file) = self
            .chat_outputs()
            .find(|file| file.id == id)
            .map(ToOwned::to_owned)
        else {
            return;
        };
        self.download_file(session, file, cx);
    }

    /// A header click flipping the section `pick` chooses.
    fn section_toggle(pick: fn(&mut CardSections) -> &mut bool, cx: &Context<Self>) -> Toggle {
        Box::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
            let open = pick(&mut this.chat_ui.session_sections);
            *open = !*open;
            cx.notify();
        }))
    }

    /// Moves the section `key` to `to` in the card's order and saves the order for every chat.
    fn move_section(&mut self, key: CardKey, to: usize, cx: &mut Context<Self>) {
        let order = &mut self.chat_ui.session_sections.order;
        if !key.move_to(order, to) {
            return;
        }
        let ids = order.map(|key| key.id().to_owned());
        self.save_preferences(
            "the session card order",
            |preferences| preferences.session_card_order = ids.into(),
            cx,
        );
        cx.notify();
    }

    /// Lays the card out in `ids`' order, as a saved order would.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_section_order(&mut self, ids: &[&str]) {
        self.chat_ui.session_sections.order = CardKey::restored(ids);
    }

    /// Shows `session`'s card, whether or not an earlier scenario left it open.
    #[cfg(feature = "snapshot")]
    pub(in crate::ui) fn fixture_show_session_card(
        &mut self,
        session: &SessionId,
        cx: &mut Context<Self>,
    ) {
        self.set_session_card(session, true, cx);
    }

    /// The Outputs section's rows: each file the Bot sent back, opening it on click.
    fn output_rows(&self, session: &SessionId, cx: &Context<Self>) -> Option<AnyElement> {
        let p = self.palette;
        let enabled = self.client.link.is_ready();
        let rows: Vec<_> = self
            .chat_outputs()
            .map(|file| {
                let id: Arc<str> = file.id.as_str().into();
                let session = session.share();
                div()
                    .id(SharedString::from(format!("session-output:{id}")))
                    .flex()
                    .items_center()
                    .gap(space::S)
                    .h(height::COMPACT)
                    .px(space::S)
                    .mx(-space::S)
                    .control_corners()
                    .typography(Type::Caption)
                    .child(icon(file_glyph(&file.media_type), &p).size(glyph::INLINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(file.name.as_str().to_owned()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(p.muted)
                            .child(file_detail(&file.name, file.size)),
                    )
                    .when(enabled, |row| {
                        row.role(gpui::Role::Button)
                            .aria_label(format!("Open file {}", file.name))
                            .focusable()
                            .tab_stop(true)
                            .cursor_pointer()
                            .hover(move |row| row.bg(p.hover))
                            .focus_visible(move |row| row.bg(p.accent_soft))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.open_chat_output(session.share(), &id, cx);
                            }))
                    })
            })
            .collect();
        (!rows.is_empty()).then(|| {
            div()
                .id("session-card-outputs-list")
                .max_h(height::LIST_MAX)
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .children(rows)
                .into_any_element()
        })
    }

    /// Where and what runs the chat, then the Bot's composer settings, as one row each.
    fn environment_section(
        &self,
        session: &SessionReadyPayload,
        cx: &Context<Self>,
    ) -> CardSection {
        let p = self.palette;
        let open = self.chat_ui.session_sections.environment;
        let row = |label: SharedString, value: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(space::S)
                .typography(Type::Caption)
                .child(div().flex_1().text_color(p.muted).child(label))
                .child(
                    div()
                        .min_w_0()
                        .max_w(px(170.))
                        .flex()
                        .justify_end()
                        .child(value),
                )
        };
        let identity = self
            .session_identity(session)
            .into_iter()
            .map(|(label, value)| {
                row(
                    label.into(),
                    div().truncate().child(value).into_any_element(),
                )
            });
        let settings = self
            .composer_setting_menus(cx)
            .into_iter()
            .chain(self.session_memory_toggle(session, cx))
            .map(|(label, menu)| row(label.into(), menu));
        CardSection {
            key: CardKey::Environment,
            summary: None,
            disclosure: Some((open, Self::section_toggle(|s| &mut s.environment, cx))),
            lead: None,
            body: Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S)
                    .children(identity)
                    .children(settings)
                    .into_any_element(),
            ),
        }
    }

    /// Whether this chat remembers: its own override, else its Bot's setting. Switching it
    /// back to the Bot's setting drops the override.
    fn session_memory_toggle(
        &self,
        session: &SessionReadyPayload,
        cx: &Context<Self>,
    ) -> Option<(String, AnyElement)> {
        let bot = self.composer_bot()?;
        let follows = bot.capabilities.memory.enabled;
        let on = session.memory.unwrap_or(follows);
        let pending = self.chat_ui.action_requests.contains_key(MEMORY_REQUEST);
        let session_id = session.session.session_id.share();
        let label = if on {
            "Turn memory off for this chat"
        } else {
            "Turn memory on for this chat"
        };
        let switch = toggle("session-memory", label, on, cx)
            .disabled(pending || !self.client.link.is_ready())
            .on_click(cx.listener(move |this, on: &bool, _, cx| {
                let request_id = crate::gateway::request_id();
                this.chat_ui
                    .action_requests
                    .insert(MEMORY_REQUEST.to_owned(), Arc::clone(&request_id));
                this.send(
                    ClientMessage::SetSessionMemory {
                        request_id,
                        session_id: session_id.share(),
                        memory: (*on != follows).then_some(*on),
                    },
                    cx,
                );
            }));
        Some(("Memory".to_owned(), switch.into_any_element()))
    }

    /// The files the Bot sent back, or a line saying where they will show.
    fn outputs_section(&self, session: &SessionReadyPayload, cx: &Context<Self>) -> CardSection {
        let p = self.palette;
        let open = self.chat_ui.session_sections.outputs;
        let id = session.session.session_id.share();
        let outputs = self.output_rows(&id, cx).unwrap_or_else(|| {
            div()
                .typography(Type::Caption)
                .text_color(p.muted)
                .child("Files the Bot sends back show here.")
                .into_any_element()
        });
        CardSection {
            key: CardKey::Outputs,
            summary: None,
            disclosure: Some((open, Self::section_toggle(|s| &mut s.outputs, cx))),
            lead: None,
            body: Some(outputs),
        }
    }

    /// Claude's session panel pinned beside the conversation: stacked sections for what
    /// runs the chat, how full its context is, the files the Bot sent back and the run totals.
    pub(super) fn session_card(
        &mut self,
        window: &Window,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let shown = self.track_session_card_motion(window, cx);
        let session = self.client.state.session.as_ref().filter(|session| {
            let id = session.session.session_id.as_ref();
            self.chat_ui.session_card.as_deref() == Some(id)
                || self.chat_ui.session_card_motion.contains(id)
        })?;
        let p = self.palette;
        let open = &self.chat_ui.session_sections;
        let metrics = session_metrics(session, self.client.state.token_usage.as_deref(), now_ms());
        let environment = self.environment_section(session, cx);
        let breakdown = self
            .client
            .state
            .token_usage
            .as_ref()
            .and_then(|usage| usage.context.as_ref());
        let (used, bar, rows) = context_parts(&metrics, breakdown, &p);
        let context = CardSection {
            key: CardKey::Context,
            summary: Some(used.into()),
            disclosure: rows
                .is_some()
                .then(|| (open.context, Self::section_toggle(|s| &mut s.context, cx))),
            lead: Some(bar.into_any_element()),
            body: rows.map(IntoElement::into_any_element),
        };
        let outputs = self.outputs_section(session, cx);
        let totals = CardSection {
            key: CardKey::Totals,
            summary: Some(compact_duration(metrics.elapsed_ms).into()),
            disclosure: None,
            lead: None,
            body: Some(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x(space::L)
                    .gap_y(space::XS)
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .children(self.session_totals(session))
                    .into_any_element(),
            ),
        };
        let order = self.chat_ui.session_sections.order;
        let mut sections = [environment, context, outputs, totals];
        sections.sort_by_key(|section| order.iter().position(|key| *key == section.key));
        // Each slot's drop lines fill the gap between tiles, so the stack sheds the outer halves.
        let card = div()
            .id("session-card")
            .occlude()
            .w(width::POPOVER)
            .my(-space::XS)
            .flex()
            .flex_col()
            .children(
                sections
                    .into_iter()
                    .enumerate()
                    .map(|(at, section)| section.render(at, &p, cx)),
            );
        let full = width::POPOVER + space::S;
        Some(
            div()
                .id("session-card-scroll")
                .relative()
                .w(shown.unwrap_or(full))
                .min_h_0()
                .flex_shrink_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .w(full)
                        .min_w(full)
                        .py(space::S)
                        .pr(space::S)
                        .child(card),
                )
                .map(|view| {
                    #[cfg(feature = "snapshot")]
                    let view = view
                        .child(snapshot::track("motion:session-card"))
                        .child(snapshot::probe("session-card-scroll".into()));
                    view
                })
                .into_any_element(),
        )
    }
}

type Toggle = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// Which session card sections are open, and the order they stack in.
pub(in crate::ui) struct CardSections {
    pub(in crate::ui) environment: bool,
    pub(in crate::ui) context: bool,
    pub(in crate::ui) outputs: bool,
    order: [CardKey; 4],
}

impl Default for CardSections {
    fn default() -> Self {
        Self {
            environment: true,
            context: false,
            outputs: true,
            order: CardKey::ALL,
        }
    }
}

impl ChatUi {
    /// The chat state a launch starts with: the session card in its saved `order`.
    pub(in crate::ui) fn restored(order: &[String]) -> Self {
        Self {
            session_sections: CardSections {
                order: CardKey::restored(order),
                ..CardSections::default()
            },
            ..Self::default()
        }
    }
}

/// The session card's sections, by the id their saved order uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CardKey {
    Environment,
    Context,
    Outputs,
    Totals,
}

impl CardKey {
    const ALL: [Self; 4] = [
        Self::Environment,
        Self::Context,
        Self::Outputs,
        Self::Totals,
    ];

    fn id(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::Context => "context",
            Self::Outputs => "outputs",
            Self::Totals => "totals",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Environment => "Environment",
            Self::Context => "Context",
            Self::Outputs => "Outputs",
            Self::Totals => "Session",
        }
    }

    /// Moves `self` to place `to` in `order`, shifting the sections between; false when
    /// it is already there.
    fn move_to(self, order: &mut [Self; 4], to: usize) -> bool {
        let Some(from) = order.iter().position(|placed| *placed == self) else {
            return false;
        };
        let moved = match from.cmp(&to) {
            std::cmp::Ordering::Less => order.get_mut(from..=to).map(|span| span.rotate_left(1)),
            std::cmp::Ordering::Greater => {
                order.get_mut(to..=from).map(|span| span.rotate_right(1))
            }
            std::cmp::Ordering::Equal => None,
        };
        moved.is_some()
    }

    /// The saved `ids` first, each once; unknown ids are skipped and missing sections
    /// follow in their default order.
    fn restored(ids: &[impl AsRef<str>]) -> [Self; 4] {
        let mut order = Self::ALL;
        let mut placed = 0;
        for id in ids {
            let Some(span) = order.get_mut(placed..).and_then(|rest| {
                let found = rest.iter().position(|key| key.id() == id.as_ref())?;
                rest.get_mut(..=found)
            }) else {
                continue;
            };
            span.rotate_right(1);
            placed += 1;
        }
        order
    }
}

/// Reserved beside the section title, so hover never shifts the header.
const HANDLE: Size<Pixels> = Size {
    width: space::L,
    height: height::MINI,
};
/// The drop line, and the room around it, on each side of the gap between two tiles.
const DROP_LINE: Pixels = px(2.);

/// A section being dragged by its handle, drawn as its lifted header.
#[derive(Clone, Copy)]
struct SectionDrag {
    key: CardKey,
    /// Its place in the order when the drag began.
    from: usize,
    palette: Palette,
}

impl Render for SectionDrag {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = self.palette;
        let title = self.key.title();
        let line = window.text_system().shape_line(
            title.into(),
            Type::Subhead.size(),
            &[TextRun {
                len: title.len(),
                font: gpui::Font {
                    weight: FontWeight::SEMIBOLD,
                    ..font(FontSet::current().regular())
                },
                ..Default::default()
            }],
            None,
        );
        let inset = space::L + px(1.);
        // Keep the lifted title just before the handle that the pointer grabbed.
        let tile = div()
            .absolute()
            .w(line.width.ceil() + space::XS + HANDLE.width + inset * 2.)
            .top(-inset)
            .right(-(HANDLE.width + inset))
            .p(space::L);
        div().child(
            backdrop::floating(tile, p.float, cx)
                .text_color(p.text)
                .border_1()
                .border_color(p.separator)
                .shadow_md()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::XS)
                        .child(div().typography(Type::Subhead).child(title))
                        .child(handle_glyph(&p)),
                ),
        )
    }
}

impl SectionDrag {
    fn handle(self, group: SharedString, cx: &Context<Workspace>) -> Stateful<Div> {
        let p = self.palette;
        handle_glyph(&p)
            .id(SharedString::from(format!(
                "session-card-handle:{}",
                self.key.id()
            )))
            .role(gpui::Role::Button)
            .aria_label(format!("Reorder {}", self.key.title()))
            .aria_description("Use the Up and Down Arrow keys to move this section.")
            .focusable()
            .tab_stop(true)
            .cursor(gpui::CursorStyle::OpenHand)
            .opacity(0.)
            .group_hover(group, |handle| handle.opacity(1.))
            .focus_visible(move |handle| handle.opacity(1.).bg(p.accent_soft))
            .map(|handle| {
                #[cfg(feature = "snapshot")]
                let handle = handle.relative().child(snapshot::probe(format!(
                    "session-card-handle:{}",
                    self.key.id()
                )));
                handle
            })
            .on_click(|_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                let to = match event.keystroke.key.as_str() {
                    "up" => self.from.saturating_sub(1),
                    "down" => (self.from + 1).min(CardKey::ALL.len() - 1),
                    _ => return,
                };
                this.move_section(self.key, to, cx);
                cx.stop_propagation();
            }))
            .on_drag(self, |drag, _, _, cx| cx.new(|_| *drag))
    }
}

fn handle_glyph(p: &Palette) -> Div {
    div()
        .flex_shrink_0()
        .w(HANDLE.width)
        .h(HANDLE.height)
        .flex()
        .items_center()
        .justify_center()
        .control_corners()
        .child(icon("hi.more01", p).size(glyph::MARK))
}

/// One of the card's stacked sections, filled like the sidebar: a title row whose chevron
/// folds `body` away, with `summary` beside the title and `lead` shown even while folded.
struct CardSection {
    key: CardKey,
    summary: Option<SharedString>,
    /// Whether the body shows, and the header click flipping it; without one it always shows.
    disclosure: Option<(bool, Toggle)>,
    lead: Option<AnyElement>,
    body: Option<AnyElement>,
}

impl CardSection {
    /// The section's tile at place `at`, with the handle its hover reveals and the drop
    /// lines a dragged section lights: above when it would move up, below when down.
    fn render(self, at: usize, p: &Palette, cx: &Context<Workspace>) -> AnyElement {
        let id = self.key.id();
        let group = SharedString::from(format!("session-card-section:{id}"));
        let open = self.disclosure.as_ref().is_none_or(|(open, _)| *open);
        let drag = SectionDrag {
            key: self.key,
            from: at,
            palette: *p,
        };
        let handle = drag.handle(SharedString::clone(&group), cx);
        let header = div()
            .id(SharedString::from(format!("session-card-{id}")))
            .flex()
            .items_center()
            .gap(space::XS)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(space::XS)
                    .typography(Type::Subhead)
                    .child(self.key.title())
                    .child(handle),
            )
            .children(self.summary.map(|summary| {
                div()
                    .typography(Type::Caption)
                    .text_color(p.muted)
                    .child(summary)
            }));
        let header = match self.disclosure {
            Some((open, toggle)) => header
                .role(gpui::Role::Button)
                .aria_label(self.key.title())
                .aria_expanded(open)
                .focusable()
                .tab_stop(true)
                .control_corners()
                .focus_visible(|header| header.bg(p.accent_soft))
                .cursor_pointer()
                .child(chevron(open, p))
                .on_click(toggle),
            None => header,
        };
        #[cfg(feature = "snapshot")]
        let header = header
            .relative()
            .child(snapshot::probe(format!("session-card-header:{id}")));
        let tile = div().relative().p(space::L).flex().flex_col().group(group);
        let tile = backdrop::floating(tile, p.float, cx)
            .border_1()
            .border_color(p.separator)
            .shadow_sm()
            .child(header)
            .children(self.lead.map(|lead| div().pt(space::S).child(lead)))
            .children(self.body.map(|body| {
                motion::smooth(format!("session-card:{id}"), div().pt(space::S).child(body))
                    .open(open)
            }));
        let accent = p.accent;
        let line = move |style: StyleRefinement, lit: bool| {
            if lit {
                style.border_color(accent)
            } else {
                style
            }
        };
        let slot = div()
            .border_t(DROP_LINE)
            .pt(DROP_LINE)
            .border_color(gpui::transparent_black())
            .drag_over::<SectionDrag>(move |style, drag, _, _| line(style, drag.from > at))
            .on_drop(cx.listener(move |this, drag: &SectionDrag, _, cx| {
                this.move_section(drag.key, at, cx);
            }))
            .child(
                div()
                    .border_b(DROP_LINE)
                    .pb(DROP_LINE)
                    .border_color(gpui::transparent_black())
                    .drag_over::<SectionDrag>(move |style, drag, _, _| line(style, drag.from < at))
                    .child(tile),
            );
        motion::slide(format!("session-card-slot:{id}"), slot).into_any_element()
    }
}

/// The context's "used / window · share" summary, its fill as a stacked bar, and a row per
/// part when the gateway sent a breakdown of the last request.
fn context_parts(
    metrics: &SessionMetrics,
    breakdown: Option<&ContextBreakdown>,
    p: &Palette,
) -> (String, Div, Option<Div>) {
    let count = |value: Option<i64>| value.map_or_else(|| "—".into(), compact_count);
    let mut used = format!("{} / {}", count(metrics.context), count(metrics.limit));
    if let Some(fraction) = metrics.fraction {
        used = format!("{used} · {}", percent(fraction.into()));
    }
    let window = metrics.limit.unwrap_or(0).max(1).to_f32();
    let counted = breakdown.into_iter().flat_map(|context| {
        context
            .parts
            .iter()
            .filter(|part| part.kind != ContextPartKind::DeferredTools)
    });
    let segments: Vec<_> = match breakdown {
        Some(_) => counted
            .map(|part| (part.tokens, part_tone(part.kind, p)))
            .collect(),
        None => vec![(metrics.context.unwrap_or(0), p.accent)],
    };
    let bar = div()
        .flex()
        .h(px(4.))
        .w_full()
        .pill_corners()
        .overflow_hidden()
        .bg(p.hover)
        .children(segments.into_iter().map(|(tokens, tone)| {
            div()
                .h_full()
                .w(relative((tokens.to_f32() / window).min(1.)))
                .bg(tone)
        }));
    let rows = breakdown.map(|context| context_rows(context, window, p));
    (used, bar, rows)
}

/// A row per part with its tokens and share of the window, then the compaction buffer and
/// what is still free. Deferred tools are listed but take no share.
fn context_rows(context: &ContextBreakdown, window: f32, p: &Palette) -> Div {
    let share = |tokens: i64| percent(f64::from(tokens.to_f32() / window));
    let row = |tone: Hsla, label: &'static str, tokens: i64, share: String| {
        div()
            .flex()
            .items_center()
            .gap(space::S)
            .child(status_dot(tone))
            .child(div().flex_1().text_color(p.muted).child(label))
            .child(compact_count(tokens))
            .child(
                div()
                    .w(px(44.))
                    .text_right()
                    .text_color(p.muted)
                    .child(share),
            )
    };
    let used: i64 = context
        .parts
        .iter()
        .filter(|part| part.kind != ContextPartKind::DeferredTools)
        .map(|part| part.tokens)
        .sum();
    let free = (window.saturating_i64() - used - context.compaction_buffer).max(0);
    div()
        .flex()
        .flex_col()
        .gap(space::XS)
        .typography(Type::Caption)
        .children(context.parts.iter().map(|part| {
            let share = match part.kind {
                ContextPartKind::DeferredTools => "—".into(),
                _ => share(part.tokens),
            };
            row(
                part_tone(part.kind, p),
                part_label(part.kind),
                part.tokens,
                share,
            )
        }))
        .child(row(
            p.muted.opacity(alpha::DIM),
            "Compaction buffer",
            context.compaction_buffer,
            share(context.compaction_buffer),
        ))
        .child(row(p.hover, "Free", free, share(free)))
}

fn part_label(kind: ContextPartKind) -> &'static str {
    match kind {
        ContextPartKind::Instructions => "Instructions",
        ContextPartKind::Skills => "Skills",
        ContextPartKind::Capabilities => "Capabilities",
        ContextPartKind::Tools => "Tools",
        ContextPartKind::Messages => "Messages",
        ContextPartKind::ToolCalls => "Tool calls",
        ContextPartKind::ToolResults => "Tool results",
        ContextPartKind::Memory => "Memory",
        ContextPartKind::Reasoning => "Reasoning",
        ContextPartKind::DeferredTools => "Tools to load",
        _ => "Other",
    }
}

fn part_tone(kind: ContextPartKind, p: &Palette) -> Hsla {
    let accent = match kind {
        ContextPartKind::Messages => Accent::Blue,
        ContextPartKind::ToolResults => Accent::Teal,
        ContextPartKind::ToolCalls => Accent::Green,
        ContextPartKind::Tools => Accent::Orange,
        ContextPartKind::Capabilities => Accent::Yellow,
        ContextPartKind::Instructions => Accent::Purple,
        ContextPartKind::Skills => Accent::Red,
        ContextPartKind::Memory => Accent::White,
        _ => return p.muted,
    };
    accent.color()
}

/// A session card value that opens a menu: the value in the row's own text, then a quiet
/// chevron, sitting flush with the values above it.
pub(super) fn card_menu_trigger(
    id: impl Into<ElementId>,
    value: impl Into<SharedString>,
    p: &Palette,
) -> Button {
    let value = value.into();
    Button::new(id)
        .ghost()
        .h(height::MINI)
        .my(-space::XXS)
        .px(space::XS)
        .mr(-space::XS)
        .accessibility_label(SharedString::clone(&value))
        .child(
            div()
                .flex()
                .items_center()
                .gap(space::XS)
                .typography(Type::Caption)
                .text_color(p.text)
                .child(div().truncate().child(value))
                .child(chevron(false, p)),
        )
}

#[cfg(test)]
mod tests {
    use super::CardKey::{self, Context, Environment, Outputs, Totals};

    #[test]
    fn saved_order_skips_unknown_and_repeated_ids_and_appends_missing_sections() {
        let none: [&str; 0] = [];
        assert_eq!(CardKey::restored(&none), CardKey::ALL);
        assert_eq!(
            CardKey::restored(&["outputs", "retired", "totals", "outputs"]),
            [Outputs, Totals, Environment, Context]
        );
        assert_eq!(
            CardKey::restored(&["totals", "outputs", "context", "environment"]),
            [Totals, Outputs, Context, Environment]
        );
    }

    #[test]
    fn a_dropped_section_takes_the_target_place() {
        let mut order = CardKey::ALL;
        assert!(Totals.move_to(&mut order, 0));
        assert_eq!(order, [Totals, Environment, Context, Outputs]);
        assert!(Environment.move_to(&mut order, 3));
        assert_eq!(order, [Totals, Context, Outputs, Environment]);
        assert!(!Outputs.move_to(&mut order, 2));
        let ids = order.map(CardKey::id);
        assert_eq!(CardKey::restored(&ids), order);
    }
}
