//! The routine editor's state: its choices, its inputs and the schedule they describe.

use super::schedule::{
    SimpleCron, format_local, local_zone, now, parse_clock, parse_local, schedule_summary,
    scheduled_start,
};
use super::*;
use gpui_component::input::{InputEvent, InputState, TextareaState};
use gpui_component::searchable_list::SearchableVec;
use gpui_component::select::SelectState;
use mobius_gateway::wire::{
    HookSelector, Routine, RoutineAction, RoutineBinding, RoutineSchedule, RoutineScheduleKind,
};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Mode {
    Once,
    Every,
    Daily,
    Weekly,
    Advanced,
    Bindings,
}

pub(super) const MODES: [(Mode, &str); 6] = [
    (Mode::Once, "Once"),
    (Mode::Every, "Every"),
    (Mode::Daily, "Daily"),
    (Mode::Weekly, "Weekly"),
    (Mode::Advanced, "Advanced cron"),
    (Mode::Bindings, "Existing triggers"),
];
pub(super) const UNITS: [(&str, u64); 3] = [("Seconds", 1), ("Minutes", 60), ("Hours", 3_600)];
pub(super) const DURATIONS: [(&str, i64); 3] =
    [("Minutes", 60), ("Hours", 3_600), ("Days", 86_400)];
pub(super) const ENDS: [&str; 3] = ["Never", "After duration", "At date"];

/// The dialog's choices; its text lives in `Inputs`.
pub(in crate::ui) struct RoutineForm {
    pub(super) inputs: Inputs,
    pub(super) focus: Option<FocusHandle>,
    pub(super) save: SaveState,
    pub(super) workspace: SharedString,
    pub(super) mode: Mode,
    pub(super) unit: usize,
    pub(super) weekday: u32,
    pub(super) ends: usize,
    pub(super) duration_unit: usize,
    /// Bindings retained when this existing routine has no timer the editor changes.
    pub(super) saved_triggers: Option<usize>,
}

pub(super) enum SaveState {
    Editing,
    Pending,
    Saved,
}

pub(super) struct Inputs {
    pub(super) instructions: Entity<TextareaState>,
    pub(super) at: Entity<InputState>,
    pub(super) every: Entity<InputState>,
    pub(super) time: Entity<InputState>,
    pub(super) cron: Entity<InputState>,
    pub(super) duration: Entity<InputState>,
    pub(super) end_at: Entity<InputState>,
    pub(super) zone: Entity<SelectState<SearchableVec<String>>>,
}

/// Invalid or overflowing numeric input follows the same disabled-save path as other form errors.
fn interval_seconds(text: &str, unit: u64) -> Option<u64> {
    text.parse::<u64>()
        .ok()?
        .checked_mul(unit)
        .filter(|seconds| *seconds >= 60)
}

fn duration_end(text: &str, unit: i64, now: i64) -> Option<i64> {
    now.checked_add(text.parse::<i64>().ok()?.checked_mul(unit)?)
}

fn text(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

impl RoutineForm {
    pub(super) fn should_close(&self, window: &Window, cx: &App) -> bool {
        matches!(self.save, SaveState::Saved)
            && self
                .focus
                .as_ref()
                .is_some_and(|focus| focus.contains_focused(window, cx))
    }

    pub(super) fn busy(&self) -> bool {
        matches!(self.save, SaveState::Pending | SaveState::Saved)
    }

    /// The routine's own choices, or a daily schedule in the first workspace.
    pub(super) fn new(
        routine: Option<&Routine>,
        workspaces: &[SharedString],
        inputs: Inputs,
    ) -> Self {
        let timer = routine.and_then(|routine| scheduled_start(&routine.bindings));
        let schedule = timer.map(|(_, schedule, _)| schedule);
        let parsed = schedule
            .and_then(|schedule| schedule.expression.as_deref())
            .and_then(SimpleCron::parse);
        let mode = match schedule.map(|schedule| schedule.kind) {
            Some(RoutineScheduleKind::Once) => Mode::Once,
            Some(RoutineScheduleKind::Interval) => Mode::Every,
            Some(RoutineScheduleKind::Cron) => match parsed {
                Some(SimpleCron { weekday: None, .. }) => Mode::Daily,
                Some(_) => Mode::Weekly,
                None => Mode::Advanced,
            },
            None if routine.is_some() => Mode::Bindings,
            None => Mode::Daily,
        };
        Self {
            inputs,
            focus: None,
            save: SaveState::Editing,
            workspace: routine
                .map(|routine| routine.workspace.to_string_lossy().into_owned().into())
                .or_else(|| workspaces.first().map(SharedString::from))
                .unwrap_or_default(),
            mode,
            unit: interval_unit(interval(routine)),
            weekday: parsed.and_then(|cron| cron.weekday).unwrap_or(1),
            ends: if timer.and_then(|(_, _, ends_at)| ends_at).is_some() {
                2
            } else {
                0
            },
            duration_unit: 1,
            saved_triggers: routine
                .filter(|_| timer.is_none())
                .map(|routine| routine.bindings.len()),
        }
    }
}

fn interval(routine: Option<&Routine>) -> u64 {
    routine
        .and_then(|routine| scheduled_start(&routine.bindings))
        .and_then(|(_, schedule, _)| schedule.every_seconds)
        .unwrap_or(600)
}

/// The largest unit that divides `seconds`.
fn interval_unit(seconds: u64) -> usize {
    UNITS
        .iter()
        .rposition(|(_, size)| seconds.is_multiple_of(*size))
        .unwrap_or(0)
}

/// A single-line input whose edits redraw the dialog.
fn live_field(
    value: &str,
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<InputState> {
    let input = field(value, placeholder, window, cx);
    cx.subscribe(&input, |_, _, _: &InputEvent, cx| cx.notify())
        .detach();
    input
}

impl Inputs {
    /// The routine's own text, or 9:00 and this Mac's time zone.
    pub(super) fn new(
        routine: Option<&Routine>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let timer = routine.and_then(|routine| scheduled_start(&routine.bindings));
        let schedule = timer.map(|(_, schedule, _)| schedule);
        let parsed = schedule
            .and_then(|schedule| schedule.expression.as_deref())
            .and_then(SimpleCron::parse);
        let clock = parsed.map_or_else(
            || "09:00".to_owned(),
            |cron| format!("{:02}:{:02}", cron.hour, cron.minute),
        );
        let local;
        let zone = if let Some(zone) = schedule.and_then(|schedule| schedule.time_zone.as_ref()) {
            zone
        } else {
            local = local_zone();
            &local
        };
        let seconds = interval(routine);
        let instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Describe what this Bot should do")
                .auto_grow(3, 6)
        });
        let text = routine.map_or("", |routine| routine.instructions.as_str());
        instructions.update(cx, |input, cx| input.set_value(text, window, cx));
        cx.subscribe(&instructions, |_, _, _: &InputEvent, cx| cx.notify())
            .detach();
        let zones: Vec<String> = chrono_tz::TZ_VARIANTS
            .iter()
            .map(|zone| zone.name().to_owned())
            .collect();
        let zone = cx.new(|cx| {
            let mut state =
                SelectState::new(SearchableVec::new(zones), None, window, cx).searchable(true);
            state.set_selected_value(zone, window, cx);
            state
        });
        let at = schedule
            .and_then(|schedule| schedule.at)
            .unwrap_or_else(|| now() + 3_600);
        let end = timer
            .and_then(|(_, _, ends_at)| ends_at)
            .unwrap_or_else(|| now() + 86_400);
        let expression = schedule
            .and_then(|schedule| schedule.expression.as_deref())
            .unwrap_or_default();
        Self {
            instructions,
            at: live_field(&format_local(at), "YYYY-MM-DD HH:MM", window, cx),
            every: live_field(
                &UNITS
                    .get(interval_unit(seconds))
                    .map_or(seconds, |(_, size)| seconds / size)
                    .to_string(),
                "Repeat interval",
                window,
                cx,
            ),
            time: live_field(&clock, "HH:MM", window, cx),
            cron: live_field(expression, "0 9 * * 1-5", window, cx),
            duration: live_field("1", "Duration", window, cx),
            end_at: live_field(&format_local(end), "YYYY-MM-DD HH:MM", window, cx),
            zone,
        }
    }
    pub(super) fn schedule(&self, form: &RoutineForm, cx: &App) -> Option<RoutineSchedule> {
        let zone = self.zone.read(cx).selected_value();
        let schedule = |kind| RoutineSchedule {
            kind,
            at: None,
            every_seconds: None,
            expression: None,
            time_zone: None,
        };
        let cron = |expression: String| RoutineSchedule {
            expression: Some(expression),
            time_zone: Some(zone.map_or_else(local_zone, String::clone)), // rules-allow: the schedule sent to the gateway owns its time zone; the picker keeps its own
            ..schedule(RoutineScheduleKind::Cron)
        };
        match form.mode {
            Mode::Bindings => None,
            Mode::Once => parse_local(&text(&self.at, cx)).map(|at| RoutineSchedule {
                at: Some(at),
                ..schedule(RoutineScheduleKind::Once)
            }),
            Mode::Every => {
                let (_, unit) = UNITS.get(form.unit)?;
                let seconds = interval_seconds(&text(&self.every, cx), *unit)?;
                Some(RoutineSchedule {
                    every_seconds: Some(seconds),
                    ..schedule(RoutineScheduleKind::Interval)
                })
            }
            Mode::Daily | Mode::Weekly => {
                let (hour, minute) = parse_clock(&text(&self.time, cx))?;
                let weekday = (form.mode == Mode::Weekly).then_some(form.weekday);
                Some(cron(
                    SimpleCron {
                        minute,
                        hour,
                        weekday,
                    }
                    .expression(),
                ))
            }
            Mode::Advanced => {
                let expression = text(&self.cron, cx);
                (!expression.is_empty()).then(|| cron(expression))
            }
        }
    }

    /// When the routine stops; `Err` when the chosen end is invalid or already past.
    pub(super) fn ends_at(&self, form: &RoutineForm, cx: &App) -> Result<Option<i64>, ()> {
        let at = match (form.mode, form.ends) {
            (Mode::Once | Mode::Bindings, _) | (_, 0) => return Ok(None),
            (_, 1) => DURATIONS
                .get(form.duration_unit)
                .and_then(|(_, unit)| duration_end(&text(&self.duration, cx), *unit, now())),
            _ => parse_local(&text(&self.end_at, cx)),
        };
        at.filter(|at| *at > now()).map(Some).ok_or(())
    }

    pub(super) fn summary(&self, form: &RoutineForm, cx: &App) -> String {
        if form.mode == Mode::Bindings {
            return if form.saved_triggers == Some(0) {
                "Runs when you start it."
            } else {
                "Runs when its saved triggers occur."
            }
            .into();
        }
        match (self.schedule(form, cx), self.ends_at(form, cx)) {
            (Some(schedule), Ok(Some(end))) => {
                format!(
                    "{} · ends {}",
                    schedule_summary(&schedule),
                    format_local(end)
                )
            }
            (Some(schedule), _) => schedule_summary(&schedule),
            (None, _) => "Choose a valid schedule.".into(),
        }
    }
}

/// Preserve binding identities, event actions and additional timers while editing one schedule.
pub(super) fn replace_schedule(
    mut bindings: Vec<RoutineBinding>,
    schedule: Option<RoutineSchedule>,
    ends_at: Option<i64>,
) -> Vec<RoutineBinding> {
    let Some(schedule) = schedule else {
        return bindings;
    };
    let on = HookSelector::Schedule { schedule, ends_at };
    let index = scheduled_start(&bindings).map(|(index, _, _)| index);
    match index.and_then(|index| bindings.get_mut(index)) {
        Some(binding) => binding.on = on,
        None => bindings.push(RoutineBinding {
            id: crate::gateway::request_id(),
            on,
            action: RoutineAction::Start,
        }),
    }
    bindings
}

#[cfg(test)]
mod numeric_tests {
    use super::duration_end;

    #[test]
    fn routine_end_dates_reject_multiplication_and_addition_overflow() {
        assert_eq!(duration_end("2", 60, 1_000), Some(1_120));
        assert_eq!(duration_end(&i64::MAX.to_string(), 60, 1_000), None);
        assert_eq!(duration_end(&i64::MIN.to_string(), 60, 1_000), None);
        assert_eq!(duration_end("1", 1, i64::MAX), None);
    }
}

#[cfg(test)]
mod binding_tests {
    use super::{replace_schedule, scheduled_start};
    use mobius_gateway::wire::{
        HookKind, HookSelector, HookSource, RoutineAction, RoutineBinding, RoutineSchedule,
        RoutineScheduleKind,
    };

    fn timer(seconds: u64) -> RoutineSchedule {
        RoutineSchedule {
            kind: RoutineScheduleKind::Interval,
            at: None,
            every_seconds: Some(seconds),
            expression: None,
            time_zone: None,
        }
    }

    fn event() -> RoutineBinding {
        RoutineBinding {
            id: "outage".into(),
            on: HookSelector::Event {
                source: HookSource::Bot {
                    bot_id: "bot".into(),
                },
                kind: HookKind::CustomReceived,
                routine_outcome: None,
                session_outcome: None,
                custom_name: Some("outage".into()),
            },
            action: RoutineAction::Pause,
        }
    }

    #[test]
    fn editing_schedule_preserves_identity_and_other_actions() {
        let original = vec![
            event(),
            RoutineBinding {
                id: "monitor".into(),
                on: HookSelector::Schedule {
                    schedule: timer(60),
                    ends_at: Some(123),
                },
                action: RoutineAction::Start,
            },
            RoutineBinding {
                id: "stop-later".into(),
                on: HookSelector::Schedule {
                    schedule: timer(120),
                    ends_at: None,
                },
                action: RoutineAction::Stop {
                    run_id: "active-run".into(),
                },
            },
        ];
        let edited = replace_schedule(original.clone(), Some(timer(300)), None);
        assert_eq!(edited[0], original[0]);
        assert_eq!(edited[2], original[2]);
        assert_eq!(&*edited[1].id, "monitor");
        assert_eq!(edited[1].action, RoutineAction::Start);
        assert_eq!(scheduled_start(&edited).unwrap().1.every_seconds, Some(300));
    }
}
