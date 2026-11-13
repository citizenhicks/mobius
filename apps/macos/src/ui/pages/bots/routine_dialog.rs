//! The routine editor dialog (RoutineViews.swift `RoutineForm`).

use super::create::dialog_heading;
use super::routine_form::{
    DURATIONS, ENDS, Inputs, MODES, Mode, RoutineForm, SaveState, UNITS, replace_schedule,
};
use super::schedule::WEEKDAYS;
use super::*;
use crate::ui::navigation::workspace_name;
use gpui_component::Disableable;
use gpui_component::input::{InputState, NumberInput};
use gpui_component::select::Select;
use mobius_gateway::wire::{
    Routine, RoutineAction, RoutineCommand, RoutineDefinition, RoutineInteractionPolicy,
    RoutineSchedule,
};

/// A handler that edits the form and redraws the dialog.
fn edit<E: 'static>(
    form: &Entity<RoutineForm>,
    update: impl Fn(&mut RoutineForm, &E) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let form = Entity::clone(form);
    move |event, window, cx| {
        form.update(cx, |form, _| {
            if !form.busy() {
                update(form, event);
            }
        });
        window.refresh();
    }
}

fn menu<T: Copy + PartialEq + 'static>(
    id: &'static str,
    form: &Entity<RoutineForm>,
    options: impl IntoIterator<Item = (T, String)>,
    current: T,
    set: fn(&mut RoutineForm, T),
    cx: &App,
) -> AnyElement {
    let mut label = String::new();
    let items = options
        .into_iter()
        .map(|(value, text)| {
            if value == current {
                label.clone_from(&text);
            }
            MenuChoice::new(
                text,
                value == current,
                edit(form, move |form, _: &ClickEvent| set(form, value)),
            )
        })
        .collect();
    menu_select(id, label, None, items, cx).into_any_element()
}

impl Workspace {
    pub(in crate::ui) fn routine_dialog(
        &self,
        bot_id: Arc<str>,
        routine: Option<Rc<Routine>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<RoutineForm> {
        let workspaces = self.routine_workspaces();
        let inputs = Inputs::new(routine.as_deref(), window, cx);
        let form = cx.new(|_| RoutineForm::new(routine.as_deref(), &workspaces, inputs));
        let editor = Entity::clone(&form);
        let pauses = self.client.state.bot(&bot_id).is_some_and(|bot| {
            bot.routine_interaction_policy == RoutineInteractionPolicy::MayPauseForApproval
        });
        let (view, epoch, ready) = (
            cx.entity().downgrade(),
            self.client.epoch,
            self.client.link.is_ready(),
        );
        window.open_dialog(cx, move |dialog, window, cx| {
            let p = *cx.global::<crate::ui::theme::Palette>();
            let close = form.read(cx).should_close(window, cx);
            if close {
                let editor = form.downgrade();
                window.defer(cx, move |window, cx| {
                    // A delayed acknowledgement must not dismiss a newer dialog.
                    if let Some(editor) = editor.upgrade()
                        && editor.read(cx).should_close(window, cx) {
                        window.close_dialog(cx);
                    }
                });
            }
            let f = form.read(cx);
            let inputs = &f.inputs;
            let (schedule, ends_at) = (inputs.schedule(f, cx), inputs.ends_at(f, cx));
            let instructions = inputs.instructions.read(cx).value().trim().to_owned();
            let valid = ready && !f.busy() && !f.workspace.is_empty() && !instructions.is_empty()
                && (schedule.is_some() || (f.mode == Mode::Bindings && f.saved_triggers.is_some()))
                && ends_at.is_ok();
            let title = if routine.is_some() { "Edit routine" } else { "New routine" };
            let submit = RoutineSubmit {
                bot_id: Arc::clone(&bot_id),
                routine: routine.as_ref().map(Rc::clone),
                workspace: SharedString::clone(&f.workspace),
                instructions,
                schedule,
                ends_at: ends_at.unwrap_or(None),
            };
            let view = WeakEntity::clone(&view);
            let editor = Entity::clone(&form);
            dialog
                .w(width::DIALOG_FORM)
                .child(dialog_body().child(dialog_heading("hi.clock", title, inputs.summary(f, cx), &p))
                .child(labelled("Instructions", textarea(&inputs.instructions).aria_label("Routine instructions").disabled(f.busy()), p.muted))
                .child(routine_rows(&form, inputs, &workspaces, cx))
                .when_some(match &f.save { SaveState::Rejected(error) => Some(error), _ => None }, |body, error| body.child(div().typography(Type::Caption).text_color(p.danger).child(SharedString::from(Arc::clone(error)))))
                .when(pauses, |dialog| {
                    dialog.child(
                        div()
                            .typography(Type::Caption)
                            .text_color(p.warning)
                            .child("This routine may pause: approval-required actions wait for you before it continues."),
                    )
                }))
                .map(|dialog| dialog_actions(dialog, if routine.is_some() { "Save" } else { "Create" }, valid, move |window, cx| {
                    let submit = submit.clone(); // rules-allow: the Save button may run more than once and each run sends its own copy of the form
                    view.update(cx, |this, cx| {
                        this.submit_gateway_dialog(epoch, cx, |this, cx| this.save_routine(submit, &editor, cx))
                    })
                    .unwrap_or(false);
                    window.refresh();
                    false
                }))
        });
        let focus = window.focused(cx);
        editor.update(cx, |form, _| form.focus = focus);
        editor
    }

    fn save_routine(
        &mut self,
        submit: RoutineSubmit,
        form: &Entity<RoutineForm>,
        cx: &mut Context<Self>,
    ) {
        if !form.update(cx, |form, _| {
            if form.busy() {
                return false;
            }
            form.save = SaveState::Pending;
            true
        }) {
            return;
        }
        let request_id = crate::gateway::request_id();
        let bindings = replace_schedule(
            submit
                .routine
                .as_ref()
                .map_or_else(Vec::new, |routine| routine.bindings.clone()), // rules-allow: the edit replaces the schedule in a copy; the shown routine stays as it was
            submit.schedule,
            submit.ends_at,
        );
        let definition = RoutineDefinition {
            workspace: String::from(submit.workspace).into(),
            instructions: submit.instructions,
            bindings,
        };
        let message = match submit.routine {
            Some(routine) => ClientMessage::RoutineCommand {
                request_id: Arc::clone(&request_id),
                command: RoutineCommand {
                    routine_id: Arc::clone(&routine.id),
                    action: RoutineAction::Update { definition },
                },
            },
            None => ClientMessage::CreateRoutine {
                request_id: Arc::clone(&request_id),
                bot_id: submit.bot_id,
                definition,
            },
        };
        self.track_bots_request(
            request_id,
            Pending::SaveRoutine(form.downgrade()),
            message,
            cx,
        );
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    /// Exercises real dialog submission and typed replies without a gateway connection.
    pub(in crate::ui) fn snapshot_routine_rejection(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use std::time::Duration;
        let editor = self.routine_dialog("fixture-mobius".into(), None, window, cx);
        editor.update(cx, |f, cx| {
            f.workspace = "/audit/routine-draft".into();
            f.mode = Mode::Advanced;
            f.ends = 2;
            f.inputs.instructions.update(cx, |input, cx| {
                input.set_value("AUDIT_ROUTINE_DRAFT", window, cx);
            });
            f.inputs
                .cron
                .update(cx, |input, cx| input.set_value("invalid cron", window, cx));
            f.inputs.zone.update(cx, |input, cx| {
                input.set_selected_value(&"Asia/Kathmandu".to_owned(), window, cx);
            });
        });
        let end = editor.read(cx).inputs.end_at.read(cx).value();
        cx.spawn_in(window, async move |this, cx| {
            let capture = |name: &str, window: &mut Window, cx: &mut App| {
                window.draw(cx).clear(cx);
                if let Some(dir) = std::env::var_os("MOBIUS_SNAPSHOT") {
                    let path = std::path::PathBuf::from(dir).join(name);
                    assert!(window.render_to_image().is_ok_and(|image| image.save(path).is_ok()));
                }
            };
            let submit = |window: &mut Window, cx: &mut App| {
                window.draw(cx).clear(cx);
                window.dispatch_action(Box::new(gpui_component::dialog::Confirm { secondary: false }), cx);
            };
            assert!(cx.update(submit).is_ok(), "fixture window remains open");
            cx.background_executor().timer(Duration::from_millis(100)).await;
            assert!(cx.update(|window, cx| capture("routine-submitted.png", window, cx)).is_ok());
            let reply = this.update_in(cx, |this, window, cx| {
                assert!(window.has_active_dialog(cx), "enqueue must not discard the form");
                assert!(matches!(editor.read(cx).save, SaveState::Pending), "real Confirm submits once");
                assert_eq!(this.bots.pending.len(), 1);
                let id = this.bots.pending.keys().next().map(Arc::clone).unwrap_or_default();
                this.bots_on_message(&mobius_gateway::wire::ServerMessage::Rejected {
                    request_id: Arc::clone(&id), code: "invalid_routine".into(), message: "Invalid cron expression".into(), fatal: false,
                }, window, cx);
                let f = editor.read(cx);
                assert!(matches!(&f.save, SaveState::Rejected(error) if &**error == "Invalid cron expression"));
                assert_eq!(&*f.workspace, "/audit/routine-draft");
                assert_eq!(f.inputs.instructions.read(cx).value(), "AUDIT_ROUTINE_DRAFT");
                assert_eq!(f.inputs.cron.read(cx).value(), "invalid cron");
                assert_eq!(f.inputs.zone.read(cx).selected_value().map(String::as_str), Some("Asia/Kathmandu"));
                assert_eq!(f.ends, 2);
                assert_eq!(f.inputs.end_at.read(cx).value(), end, "the end bound is preserved");
            });
            assert!(reply.is_ok(), "fixture workspace remains open");
            assert!(cx.update(|window, cx| capture("routine-rejected-inline.png", window, cx)).is_ok());
            assert!(this.update_in(cx, |_, window, cx| {
                editor.update(cx, |f, cx| f.inputs.cron.update(cx, |input, cx| input.set_value("0 9 * * *", window, cx)));
            }).is_ok());
            assert!(cx.update(submit).is_ok(), "corrected form submits");
            assert!(cx.update(submit).is_ok(), "a repeated Confirm is harmless");
            cx.background_executor().timer(Duration::from_millis(100)).await;
            let accepted = this.update_in(cx, |this, window, cx| {
                assert_eq!(this.bots.pending.len(), 1, "no duplicate Create request");
                let id = this.bots.pending.keys().next().map(Arc::clone).unwrap_or_default();
                window.open_dialog(cx, |dialog, _, _| dialog.title("Newer dialog"));
                this.bots_on_message(&mobius_gateway::wire::ServerMessage::Accepted { request_id: id }, window, cx);
                assert!(matches!(editor.read(cx).save, SaveState::Saved));
            });
            assert!(accepted.is_ok(), "fixture receives acceptance");
            assert!(cx.update(|window, cx| window.draw(cx).clear(cx)).is_ok());
            cx.background_executor().timer(Duration::from_millis(100)).await;
            assert!(cx.update(|window, cx| {
                assert!(window.has_active_dialog(cx), "acceptance must not close the newer dialog");
                window.close_dialog(cx);
                window.draw(cx).clear(cx);
            }).is_ok());
            cx.background_executor().timer(Duration::from_millis(100)).await;
            assert!(cx.update(|window, cx| {
                assert!(!window.has_active_dialog(cx), "the accepted editor closes after the newer dialog leaves");
            }).is_ok());
        }).detach();
    }
}

#[derive(Clone)]
struct RoutineSubmit {
    bot_id: Arc<str>,
    routine: Option<Rc<Routine>>,
    workspace: SharedString,
    instructions: String,
    schedule: Option<RoutineSchedule>,
    ends_at: Option<i64>,
}

/// Workspace, repeat and the controls of the chosen schedule and cutoff.
fn routine_rows(
    form: &Entity<RoutineForm>,
    inputs: &Inputs,
    workspaces: &[SharedString],
    cx: &App,
) -> Group {
    let f = form.read(cx);
    let names = workspaces.iter().map(|path| {
        let name = workspace_name(path);
        (SharedString::clone(path), name)
    });
    let workspace_items = names
        .map(|(path, name)| {
            let checked = path == f.workspace;
            MenuChoice::new(
                name,
                checked,
                edit(form, move |form, _: &ClickEvent| {
                    form.workspace = SharedString::clone(&path);
                }),
            )
        })
        .collect();
    let workspace_label = match workspace_name(&f.workspace) {
        "" => "Choose…",
        name => name,
    }
    .to_owned();
    let modes = MODES
        .into_iter()
        .filter(|(mode, _)| *mode != Mode::Bindings || f.saved_triggers.is_some())
        .map(|(mode, label)| {
            let label = if mode == Mode::Bindings && f.saved_triggers == Some(0) {
                "Manual"
            } else {
                label
            };
            (mode, label.to_owned())
        });
    let mut group = Group::new()
        .child(
            Row::new("routine-workspace", "Workspace").control(menu_select(
                "routine-workspace-menu",
                workspace_label,
                None,
                workspace_items,
                cx,
            )),
        )
        .child(Row::new("routine-repeat", "Repeat").control(menu(
            "routine-repeat-menu",
            form,
            modes,
            f.mode,
            |form, mode| form.mode = mode,
            cx,
        )));
    group = schedule_rows(group, form, f, inputs, cx);
    group = end_rows(group, form, f, inputs);
    group
}

/// Trailing widths sized to their content: a time or a count, and a date with its time.
const SHORT_FIELD: Pixels = px(120.);
const DATE_FIELD: Pixels = px(180.);

fn sized_field(
    input: &Entity<InputState>,
    width: Pixels,
    label: &'static str,
    form: &RoutineForm,
) -> Div {
    div()
        .w(width)
        .child(text_field(input).aria_label(label).disabled(form.busy()))
}

/// The controls of the chosen repeat mode, then the time zone of clock schedules.
fn schedule_rows(
    mut group: Group,
    form: &Entity<RoutineForm>,
    f: &RoutineForm,
    inputs: &Inputs,
    cx: &App,
) -> Group {
    group = match f.mode {
        Mode::Bindings => group,
        Mode::Once => group.child(Row::new("routine-at", "Run at").control(sized_field(&inputs.at, DATE_FIELD, "Run at", f))),
        Mode::Every => group.child(
            Row::new("routine-every", "Every")
                .control(NumberInput::new(&inputs.every).w(SHORT_FIELD).disabled(f.busy()))
                .control(segmented("routine-unit", UNITS.map(|(label, _)| label), Some(f.unit), edit(form, |form, unit: &usize| form.unit = *unit))),
        ),
        Mode::Daily => group.child(Row::new("routine-time", "Time").control(sized_field(&inputs.time, SHORT_FIELD, "Time", f))),
        Mode::Weekly => {
            let days = (0u32..).zip(WEEKDAYS).map(|(day, name)| (day, name.to_owned()));
            group
                .child(Row::new("routine-day", "Day").control(menu("routine-day-menu", form, days, f.weekday, |form, day| form.weekday = day, cx)))
                .child(Row::new("routine-time", "Time").control(sized_field(&inputs.time, SHORT_FIELD, "Time", f)))
        }
        Mode::Advanced => group.child(
            StackedField::new("Cron expression")
                .control(text_field(&inputs.cron).disabled(f.busy()).aria_label("Cron expression").font_family(FontSet::current().mono()))
                .help("Five fields: minute, hour, day of month, month and weekday, in the time zone below."),
        ),
    };
    if matches!(f.mode, Mode::Daily | Mode::Weekly | Mode::Advanced) {
        group = group.child(
            Row::new("routine-zone", "Time zone").control(
                div().w(width::FIELD).child(
                    Select::new(&inputs.zone)
                        .disabled(f.busy())
                        .accessibility_label("Time zone")
                        .search_placeholder("Search time zones"),
                ),
            ),
        );
    }
    group
}

/// When a repeating routine ends.
fn end_rows(
    mut group: Group,
    form: &Entity<RoutineForm>,
    f: &RoutineForm,
    inputs: &Inputs,
) -> Group {
    if !matches!(f.mode, Mode::Once | Mode::Bindings) {
        group = group.child(Row::new("routine-ends", "Ends").control(segmented(
            "routine-ends-choice",
            ENDS,
            Some(f.ends),
            edit(form, |form, ends: &usize| form.ends = *ends),
        )));
        group = match f.ends {
            1 => group.child(
                Row::new("routine-duration", "After")
                    .leading(Leading::Gutter)
                    .control(
                        NumberInput::new(&inputs.duration)
                            .w(SHORT_FIELD)
                            .disabled(f.busy()),
                    )
                    .control(segmented(
                        "routine-duration-unit",
                        DURATIONS.map(|(label, _)| label),
                        Some(f.duration_unit),
                        edit(form, |form, unit: &usize| form.duration_unit = *unit),
                    )),
            ),
            2 => group.child(
                Row::new("routine-end-at", "End at")
                    .leading(Leading::Gutter)
                    .control(sized_field(&inputs.end_at, DATE_FIELD, "End at", f)),
            ),
            _ => group,
        };
    }
    group
}
