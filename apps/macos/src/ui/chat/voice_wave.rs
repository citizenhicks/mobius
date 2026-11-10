//! The voice composer's equalizer (RealtimeVoiceSession.swift AudioLevelEqualizer): a dotted
//! mountain that widens with the call's level. Only this view redraws: at most 30 frames
//! per second while there is sound, a slow poll while silent, and a still field under
//! Reduce Motion.

use super::*;
use crate::host::platform::VoiceLevels;
use crate::numeric::{Saturating, ToFloat};
use std::time::{Duration, Instant};

/// How often a silent or still field checks for sound.
const QUIET_POLL: Duration = Duration::from_millis(100);
/// Dot pitch of the field.
const STEP: f32 = 4.;
/// The shallow reflection under the baseline.
const REFLECTION: f32 = 0.2;

pub(super) struct VoiceWave {
    workspace: WeakEntity<Workspace>,
    levels: VoiceLevels,
    tint: Hsla,
    dark: bool,
    started: Instant,
    poll: Option<Task<()>>,
}

impl VoiceWave {
    fn set(&mut self, tint: Hsla, dark: bool, cx: &mut Context<Self>) {
        if (self.tint, self.dark) != (tint, dark) {
            (self.tint, self.dark) = (tint, dark);
            cx.notify();
        }
    }

    /// Eases toward the measured levels, as SwiftUI's `.smooth(duration: 0.09)`.
    fn follow(&mut self, target: VoiceLevels) -> bool {
        let ease = |from: f32, to: f32| {
            let next = from + (to - from) * 0.6;
            if to == 0. && next < 0.01 { 0. } else { next }
        };
        let next = VoiceLevels {
            level: ease(self.levels.level, target.level),
            flare: ease(self.levels.flare, target.flare),
            playback: target.playback,
        };
        let changed = next != self.levels;
        self.levels = next;
        changed
    }

    fn ensure_poll(&mut self, cx: &Context<Self>) {
        if self.poll.is_some() {
            return;
        }
        let workspace = WeakEntity::clone(&self.workspace);
        self.poll = Some(cx.spawn(async move |this, cx| {
            let mut frame = QUIET_POLL;
            loop {
                cx.background_executor().timer(frame).await;
                let Ok(target) = workspace.read_with(cx, |workspace, _| workspace.voice_levels())
                else {
                    return;
                };
                let alive = this.update(cx, |wave, cx| {
                    let changed = wave.follow(target);
                    let moving = wave.levels.level > 0. && !cx.reduce_motion();
                    frame = if moving { motion::FRAME } else { QUIET_POLL };
                    if changed || moving {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }
}

impl Render for VoiceWave {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.is_window_active() || cfg!(feature = "snapshot") {
            self.ensure_poll(cx);
        } else {
            self.poll = None;
        }
        let still = !motion::is_animating(window, cx) || self.levels.level == 0.;
        let time = if still {
            0.
        } else {
            self.started.elapsed().as_secs_f32()
        };
        let (levels, dark) = (self.levels, self.dark);
        let playback = levels.playback.then_some(self.tint);
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| paint_field(bounds, levels, time, playback, dark, window),
        )
        .size_full()
    }
}

/// One column's dots: a Gaussian hump widening with the amplitude, and a needle that
/// jitters on its own stable phase and speed.
fn paint_field(
    bounds: Bounds<Pixels>,
    levels: VoiceLevels,
    time: f32,
    playback: Option<Hsla>,
    dark: bool,
    window: &mut Window,
) {
    let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let amplitude = (levels.level.sqrt() * 1.5).min(1.);
    let ceiling = (height - 8.) / (1. + REFLECTION);
    let baseline = height - 4. - ceiling * REFLECTION;
    let columns = (width / STEP).saturating_usize().clamp(2, 96);
    let variance = 0.02 + 0.12 * amplitude + 0.05 * levels.flare;
    for column in 0..columns {
        let x = column.to_f32() / (columns - 1).to_f32();
        let centered = x * 2. - 1.;
        let hump = (-centered * centered / (2. * variance)).exp();
        let phase = column.to_f32() * 2.39996;
        let needle = 0.15
            + 0.85
                * (time * (2.1 + 0.9 * phase.sin()) + phase)
                    .sin()
                    .abs()
                    .powf(1.6);
        let dots = ((amplitude * 1.45).min(1.) * hump * needle * ceiling / STEP).saturating_i32();
        let below = (dots.to_f32() * REFLECTION).saturating_i32();
        for dot in -below..=dots {
            let fade = if dots == 0 {
                0.
            } else {
                dot.unsigned_abs().to_f32() / dots.to_f32()
            };
            let alpha = (0.35 + 0.65 * (1. - fade)) * (0.3 + 0.7 * hump);
            let ink = playback.unwrap_or_else(|| {
                let lightness = if dark {
                    0.92 - 0.44 * fade
                } else {
                    0.08 + 0.44 * fade
                };
                hsla(0., 0., lightness, 1.)
            });
            let radius = 0.95 - 0.4 * fade;
            let sway = (time * 1.3 + phase).sin() * 1.6 * fade;
            let origin = point(
                bounds.origin.x + px(x * (width - 4.) + 2. + sway - radius),
                bounds.origin.y + px(baseline - dot.to_f32() * STEP - radius),
            );
            window.paint_quad(pill_quad(
                Bounds::new(origin, size(px(radius * 2.), px(radius * 2.))),
                ink.opacity(alpha),
            ));
        }
    }
}

impl Workspace {
    /// The live call's levels, or the snapshot fixture's.
    pub(in crate::ui) fn voice_levels(&self) -> VoiceLevels {
        self.chat_ui
            .fixture_voice
            .unwrap_or_else(|| self.native.voice_levels())
    }

    /// The equalizer for the voice composer, tinted with the Bot's colour while it speaks.
    pub(super) fn voice_wave(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tint = self
            .composer_bot()
            .map_or(self.palette.accent, |bot| Accent::from(bot.tint).color());
        // Light text means a dark appearance.
        let dark = self.palette.text.l > 0.5;
        let workspace = cx.entity().downgrade();
        let wave = window.use_keyed_state("voice-wave", cx, |_, _| VoiceWave {
            workspace,
            levels: VoiceLevels::default(),
            tint,
            dark,
            started: Instant::now(),
            poll: None,
        });
        wave.update(cx, |wave, cx| wave.set(tint, dark, cx));
        div().h(px(104.)).w_full().child(wave).into_any_element()
    }
}
