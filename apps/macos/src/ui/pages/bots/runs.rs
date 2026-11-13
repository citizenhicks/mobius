//! The run history on a Bot's overview (RoutineViews.swift `RoutineRunRow`).

use super::Pending;
use super::menu::{MenuAction, fill_menu};
use super::schedule::run_status_label;
use crate::ui::Workspace;
use crate::ui::primitives::{
    Group, Row, Section, Type, Typography as _, ago, first_line, glyph, icon, inset, palette,
    single_line, space, status_dot,
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use gpui_component::menu::ContextMenuExt;
use mobius_gateway::wire::{ClientMessage, RoutineAction, RoutineRun, RoutineRunStatus};
use std::sync::Arc;

const RUNS_PAGE: usize = 5;

impl Workspace {
    pub(super) fn runs_section(&self, bot_id: &str, cx: &Context<Self>) -> Section {
        let mut runs: Vec<_> = self
            .client
            .state
            .routine_history
            .iter()
            .filter(|run| *run.bot_id == *bot_id)
            .collect();
        runs.sort_by_key(|run| std::cmp::Reverse(run.started_at));
        let key = format!("runs:{bot_id}");
        let shown = self.shown(&key, RUNS_PAGE);
        let mut group = Group::new();
        if runs.is_empty() {
            group = group.child(Row::empty("no-runs", "No routine runs yet."));
        }
        for run in runs.iter().take(shown) {
            group = group.child(self.run_row(run, cx));
        }
        if runs.len() > shown {
            group = group.child(Self::show_more(key, RUNS_PAGE, cx));
        }
        Section::new("Run history").child(group)
    }

    fn run_row(&self, run: &RoutineRun, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let color = match run.status {
            RoutineRunStatus::Succeeded => p.signal,
            RoutineRunStatus::Failed => p.danger,
            RoutineRunStatus::Running => p.accent,
            RoutineRunStatus::Skipped | RoutineRunStatus::Cancelled => p.muted,
        };
        let name = self
            .client
            .state
            .routines
            .iter()
            .find(|routine| routine.id == run.routine_id)
            .map_or_else(
                || "Routine".into(),
                |routine| single_line(&routine.instructions),
            );
        let awaiting = self.client.state.gateway.as_ref().is_some_and(|gateway| {
            gateway
                .background_approvals
                .iter()
                .any(|approval| run.session_id.as_deref() == Some(&*approval.session_id))
        });
        let (open, delete, running) = (
            Arc::clone(&run.id),
            Arc::clone(&run.id),
            run.status == RoutineRunStatus::Running,
        );
        let view = cx.entity().downgrade();
        let mut actions = Vec::with_capacity(2);
        if running {
            let (routine_id, run_id) = (Arc::clone(&run.routine_id), Arc::clone(&run.id));
            actions.push(
                MenuAction::new("Stop run", "hi.stopFill", move |this, _, cx| {
                    this.routine_command(
                        &routine_id,
                        RoutineAction::Stop {
                            run_id: Arc::clone(&run_id),
                        },
                        Pending::Routine,
                        cx,
                    );
                })
                .disabled(!self.client.link.is_ready()),
            );
        }
        actions.push(
            MenuAction::new("Delete run…", "hi.trash", move |this, window, cx| {
                this.delete_run(&delete, window, cx);
            })
            .disabled(running || !self.client.link.is_ready()),
        );
        div()
            .id(SharedString::from(format!("run:{}", run.id)))
            .flex()
            .items_start()
            .gap(space::S)
            .px(space::L)
            .py(inset::ROW)
            .when(run.session_id.is_none(), |row| row.opacity(0.5))
            .when(run.session_id.is_some(), |row| {
                row.cursor_pointer()
                    .hover(|style| style.bg(p.hover))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_run(&open, None, cx)))
            })
            .child(first_line(Type::Body, status_dot(color)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(space::XS)
                    .child(div().typography(Type::Body).truncate().child(name))
                    .child(
                        div()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .child(ago(run.started_at)),
                    )
                    .children(run.message.as_deref().map(|message| {
                        div()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .line_clamp(2)
                            .child(single_line(&self.provider_failure_text(message)))
                    })),
            )
            .when(awaiting, |row| {
                row.child(icon("hi.bellDot", &palette(cx)).size(glyph::MARK))
            })
            .child(
                div()
                    .typography(Type::Label)
                    .text_color(color)
                    .child(run_status_label(run.status)),
            )
            .context_menu(move |menu, _, cx| fill_menu(menu, &view, &actions, cx))
            .into_any_element()
    }

    /// Asks for the run `id`'s transcript, the page at `before` or the latest one.
    pub(in crate::ui) fn open_run(
        &mut self,
        id: &Arc<str>,
        before: Option<mobius_gateway::wire::HistoryCursor>,
        cx: &mut Context<Self>,
    ) {
        let request_id = crate::gateway::request_id();
        self.client.state.routine_preview_request = Some(Arc::clone(&request_id));
        self.send(
            ClientMessage::GetRoutineRunPreview {
                request_id,
                id: Arc::clone(id),
                before,
            },
            cx,
        );
    }

    fn delete_run(&self, id: &Arc<str>, window: &mut Window, cx: &mut Context<Self>) {
        let id = Arc::clone(id);
        self.confirm_destructive(
            "Delete this routine run?",
            "This removes the run history and its conversation transcript.",
            "Delete",
            window,
            cx,
            move |this, _, cx| {
                let request_id = crate::gateway::request_id();
                let message = ClientMessage::DeleteRoutineRun {
                    request_id: Arc::clone(&request_id),
                    id: Arc::clone(&id),
                };
                this.track_bots_request(request_id, Pending::Routine, message, cx);
            },
        );
    }
}
