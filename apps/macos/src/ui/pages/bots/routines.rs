//! Routine and run rows on a Bot's overview (RoutineViews.swift).

use super::Pending;
use super::menu::{MenuAction, more_menu};
use super::schedule::{now, routine_summary};
use crate::ui::Workspace;
use crate::ui::primitives::{
    Group, Row, Section, Spinner, Type, Typography as _, clip, compact_duration, glyph, height,
    icon, inline_button, inset, motion, palette, space,
};
use gpui::{
    AnyElement, Context, Div, InteractiveElement as _, IntoElement as _, ParentElement as _,
    SharedString, Styled as _, Window, div, px,
};
use gpui_component::Disableable as _;
use mobius_gateway::wire::{BotRecord, ClientMessage, Routine, RoutineAction, RoutineCommand};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

impl Workspace {
    /// Reloads every routine and run; the one builder of these requests.
    pub(in crate::ui) fn refresh_routines(&mut self, cx: &mut Context<Self>) {
        for message in [
            ClientMessage::ListRoutines {
                request_id: crate::gateway::request_id(),
                bot_id: None,
            },
            ClientMessage::ListRoutineHistory {
                request_id: crate::gateway::request_id(),
                id: None,
            },
        ] {
            self.send(message, cx);
        }
    }

    /// Distinct chat workspaces, the places a routine can run.
    pub(super) fn routine_workspaces(&self) -> Vec<SharedString> {
        let mut workspaces: Vec<SharedString> = self
            .client
            .state
            .sessions
            .iter()
            .filter_map(|session| session.session_context.workspace_label.as_deref())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(SharedString::from)
            .collect();
        workspaces.sort_by_key(|path| crate::ui::navigation::workspace_name(path).to_lowercase());
        workspaces
    }

    pub(super) fn routines_section(&self, bot: &BotRecord, cx: &Context<Self>) -> Section {
        let mut routines: Vec<_> = self
            .client
            .state
            .routines
            .iter()
            .filter(|routine| routine.bot_id == bot.id)
            .collect();
        routines.sort_by_key(|routine| routine.next_run_at.unwrap_or(i64::MAX));
        let mut group = Group::new();
        if routines.is_empty() {
            group = group.child(Row::empty("no-routines", "No routines yet."));
        }
        for routine in routines {
            group = group.child(self.routine_row(routine, cx));
        }
        Section::new("Routines").child(group)
    }

    fn routine_row(&self, routine: &Routine, cx: &Context<Self>) -> AnyElement {
        let p = self.palette;
        let glyph = if routine.finished {
            "hi.checkCircle"
        } else if routine.enabled {
            "hi.playFill"
        } else {
            "hi.stopFill"
        };
        let next = routine.next_run_at.map_or_else(
            || "Next —".to_owned(),
            |at| {
                let seconds = u64::try_from(at.saturating_sub(now())).unwrap_or(0);
                format!("Next in {}", compact_duration(seconds.saturating_mul(1000)))
            },
        );
        let meta = |text: String| {
            div()
                .typography(Type::Caption)
                .text_color(p.muted)
                .child(text)
        };
        div()
            .id(SharedString::from(format!("routine:{}", routine.id)))
            .flex()
            .items_start()
            .gap(space::S)
            .px(space::L)
            .py(inset::ROW)
            .min_h(px(64.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(space::XS)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(space::S)
                            .child(icon(glyph, &palette(cx)).size(glyph::INLINE))
                            .child(meta(routine_summary(routine)))
                            .child(div().flex_1())
                            .child(meta(next)),
                    )
                    .child(
                        div()
                            .typography(Type::BodyMedium)
                            .line_clamp(3)
                            .child(clip(routine.instructions.trim(), 400)),
                    )
                    .child(
                        div()
                            .typography(Type::Caption)
                            .text_color(p.muted)
                            .truncate()
                            .child(routine.workspace.to_string_lossy().into_owned()),
                    ),
            )
            .child(self.routine_controls(routine, cx))
            .into_any_element()
    }

    fn routine_controls(&self, routine: &Routine, cx: &Context<Self>) -> Div {
        let running = self.is_pending(&Pending::RunRoutine(Arc::clone(&routine.id)));
        let run_key = SharedString::from(format!("run-state:{}:{running}", routine.id));
        let run = if running {
            div()
                .size(height::COMPACT)
                .flex()
                .items_center()
                .justify_center()
                .child(Spinner::new(glyph::INLINE))
                .into_any_element()
        } else {
            let id = Arc::clone(&routine.id);
            inline_button(
                SharedString::from(format!("run:{}", routine.id)),
                "Run now",
                "hi.playFill",
                cx,
            )
            .disabled(!self.routine_runnable(routine))
            .on_click(cx.listener(move |this, _, _, cx| this.run_routine(&id, cx)))
            .into_any_element()
        };
        let run = motion::fade_in(div().child(run), run_key, motion::QUICK, cx);
        let more = inline_button(
            SharedString::from(format!("routine-more:{}", routine.id)),
            "Routine actions",
            "hi.dotsThree",
            cx,
        );
        div()
            .flex()
            .items_center()
            .gap(space::XS)
            .child(run)
            .child(more_menu(more, self.routine_actions(routine), cx))
    }

    /// Whether the routine's Bot can run here: connected, and its harness set up on the gateway.
    fn routine_runnable(&self, routine: &Routine) -> bool {
        self.client.link.is_ready()
            && self
                .client
                .state
                .bot(&routine.bot_id)
                .is_some_and(|bot| crate::ui::pages::harness::available(bot.harness))
    }

    fn routine_actions(&self, routine: &Routine) -> Vec<MenuAction> {
        let ready = self.client.link.is_ready();
        let runnable = self.routine_runnable(routine);
        let (run, toggle, delete) = (
            Arc::clone(&routine.id),
            Arc::clone(&routine.id),
            Arc::clone(&routine.id),
        );
        let enabled = routine.enabled;
        let edit = Rc::new(routine.clone()); // rules-allow: the editor opens on the routine as this menu showed it
        vec![
            MenuAction::new("Run now", "hi.playFill", move |this, _, cx| {
                this.run_routine(&run, cx);
            })
            .disabled(!runnable),
            MenuAction::new("Edit…", "hi.pencilSimple", move |this, window, cx| {
                this.routine_dialog(Arc::clone(&edit.bot_id), Some(Rc::clone(&edit)), window, cx);
            })
            .disabled(!runnable),
            MenuAction::new(
                if routine.enabled { "Pause" } else { "Resume" },
                if routine.enabled {
                    "hi.stopFill"
                } else {
                    "hi.playFill"
                },
                move |this, _, cx| this.toggle_routine(&toggle, enabled, cx),
            )
            .disabled(!runnable || routine.finished),
            MenuAction::new("Delete…", "hi.trash", move |this, window, cx| {
                this.delete_routine(&delete, window, cx);
            })
            .disabled(!ready),
        ]
    }

    fn run_routine(&mut self, id: &Arc<str>, cx: &mut Context<Self>) {
        self.routine_command(
            id,
            RoutineAction::Start,
            Pending::RunRoutine(Arc::clone(id)),
            cx,
        );
    }

    pub(super) fn routine_command(
        &mut self,
        id: &Arc<str>,
        action: RoutineAction,
        pending: Pending,
        cx: &mut Context<Self>,
    ) {
        let request_id = crate::gateway::request_id();
        let message = ClientMessage::RoutineCommand {
            request_id: Arc::clone(&request_id),
            command: RoutineCommand {
                routine_id: Arc::clone(id),
                action,
            },
        };
        self.track_bots_request(request_id, pending, message, cx);
    }

    fn toggle_routine(&mut self, id: &Arc<str>, enabled: bool, cx: &mut Context<Self>) {
        self.routine_command(
            id,
            if enabled {
                RoutineAction::Pause
            } else {
                RoutineAction::Resume
            },
            Pending::Routine,
            cx,
        );
    }

    fn delete_routine(&self, id: &Arc<str>, window: &mut Window, cx: &mut Context<Self>) {
        let id = Arc::clone(id);
        self.confirm_destructive(
            "Delete this routine?",
            "This removes the routine and its run history. This cannot be undone.",
            "Delete",
            window,
            cx,
            move |this, _, cx| {
                this.routine_command(&id, RoutineAction::Delete, Pending::Routine, cx);
            },
        );
    }
}
