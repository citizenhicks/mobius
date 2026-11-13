//! The lively face: an entity that beats, blinks and winks, turning its glide without
//! rendering and rendering only when its eyes or mood change.

use super::*;

/// A face that moves like SwiftUI's; see the module note on cost.
pub(crate) struct BotFaceView {
    tint: Hsla,
    shape: BotShape,
    size: Pixels,
    mood: BotMood,
    /// The pose shown, gliding between beats on the GPU; beats turn it without rendering.
    live: Live,
    blinking: bool,
    winking: bool,
    beat: Option<Task<()>>,
    /// Cogs turn and z's drift counting from here.
    since: Instant,
    _activation: Subscription,
}

/// Returns the lively face for `key`, updating its tint, shape and mood.
pub(crate) fn lively_face(
    key: impl Into<SharedString>,
    tint: Hsla,
    shape: BotShape,
    mood: BotMood,
    size: Pixels,
    window: &mut Window,
    cx: &mut App,
) -> Entity<BotFaceView> {
    let face = window.use_keyed_state(key.into(), cx, |window, cx| BotFaceView {
        tint,
        shape,
        size,
        mood,
        live: Live::new(Glide::still(Pose::resting(mood))),
        blinking: false,
        winking: false,
        beat: None,
        since: Instant::now(),
        _activation: cx.observe_window_activation(window, |_, _, cx| cx.notify()),
    });
    face.update(cx, |face, cx| face.set(tint, shape, mood, size, cx));
    face
}

impl BotFaceView {
    fn set(
        &mut self,
        tint: Hsla,
        shape: BotShape,
        mood: BotMood,
        size: Pixels,
        cx: &mut Context<Self>,
    ) {
        if (self.tint, self.shape, self.mood, self.size) != (tint, shape, mood, size) {
            let changed_mood = self.mood != mood;
            (self.tint, self.shape, self.mood, self.size) = (tint, shape, mood, size);
            if changed_mood {
                self.beat = None;
                self.glide(Pose::resting(mood), MOOD_SPRING, cx);
            }
            cx.notify();
        }
    }

    fn shown_mood(&self) -> BotMood {
        if self.winking {
            BotMood::Wink
        } else {
            self.mood
        }
    }

    /// Springs the pose toward `to` from wherever it shows now. The GPU draws the glide
    /// through the motions already painted, so this renders nothing; callers present it
    /// with `Window::refresh_motions` or render for their own reasons.
    fn glide(&self, to: Pose, spring: (f32, f32), cx: &App) {
        let now = Instant::now();
        self.live.set(if cx.reduce_motion() {
            Glide::still(to)
        } else {
            Glide {
                from: self.live.glide.get().at(now),
                to,
                start: now,
                spring,
            }
        });
    }

    /// One beat task per mood, as BotFace.swift's loop: spring to a new pose, hold for
    /// the mood's beat, and now and then blink before the next pose.
    fn ensure_beat(&mut self, window: &Window, cx: &Context<Self>) {
        if self.beat.is_some() || !motion::is_animating(window, cx) {
            return;
        }
        let mood = self.shown_mood();
        self.beat = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let moved = this.update_in(cx, |face, window, cx| {
                    face.glide(Pose::random(mood), POSE_SPRING, cx);
                    window.refresh_motions();
                });
                if moved.is_err() {
                    return;
                }
                let (low, high) = mood.beat();
                cx.background_executor()
                    .timer(Duration::from_secs_f32(between(low, high)))
                    .await;
                if mood.blinks() && between(0., 4.) < 1. {
                    if let Err(error) = this.update(cx, |face, cx| {
                        face.blinking = true;
                        cx.notify();
                    }) {
                        tracing::debug!(%error, "bot face released before its blink");
                        return;
                    }
                    cx.background_executor().timer(motion::BLINK).await;
                    if let Err(error) = this.update(cx, |face, cx| {
                        face.blinking = false;
                        cx.notify();
                    }) {
                        tracing::debug!(%error, "bot face released during its blink");
                        return;
                    }
                }
            }
        }));
    }

    /// Three z's rising from the crown, each growing and fading over its life. Each is
    /// laid out at the start of its life and drifts on the GPU from `since`.
    fn drifting_zs(&self, cx: &App) -> impl Iterator<Item = AnyElement> + use<> {
        let (size, ink, since) = (self.size, palette(cx).muted, self.since);
        (0..3).map(move |index| {
            let offset = index.to_f32() * Z_GAP;
            div()
                .absolute()
                .left(size * 0.78)
                .top(size * 0.18)
                .font_family(FontSet::current().lettering())
                .font_weight(FontWeight::BLACK)
                .text_size(size * 0.2 * 0.6)
                .text_color(ink)
                .child("z")
                .with_motion(
                    Motion::new(move |at| {
                        let life = (clock(since, at) + offset) % Z_LIFE / Z_LIFE;
                        let grow = (0.6 + 0.5 * life) / 0.6;
                        MotionFrame {
                            offset: point(size * 0.22 * life, size * -0.4 * life),
                            scale: gpui::size(grow, grow),
                            opacity: (life * std::f32::consts::PI).sin(),
                            ..MotionFrame::default()
                        }
                    })
                    .anchor(0., 0.)
                    .max_fps(motion::AMBIENT_FPS),
                )
                .into_any_element()
        })
    }
}

impl Render for BotFaceView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let animating = motion::is_animating(window, cx);
        if animating {
            self.ensure_beat(window, cx);
        } else {
            self.beat = None;
            self.blinking = false;
            self.live
                .set(Glide::still(Pose::resting(self.shown_mood())));
        }
        let mood = self.shown_mood();
        let ambient = (animating && self.size >= DETAIL).then_some(self.since);
        let eyes = match mood {
            _ if self.blinking => Eyes::Glyphs("- -"),
            BotMood::Thinking if self.size >= DETAIL => Eyes::Cogs(ambient),
            mood => Eyes::Glyphs(mood.eyes()),
        };
        let zs = ambient
            .filter(|_| mood == BotMood::Sleepy)
            .map(|_| self.drifting_zs(cx));
        let course = if animating {
            Course::live(self.live.for_size(self.size))
        } else {
            Course::Still(self.live.glide.get().to)
        };
        let face = face(self.tint, self.shape, self.size, eyes, &course)
            .children(zs.into_iter().flatten())
            .id("bot-face")
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|face, _, _, cx| {
                    face.winking = true;
                    face.beat = None;
                    cx.notify();
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(motion::WINK).await;
                        if let Err(error) = this.update(cx, |face, cx| {
                            face.winking = false;
                            face.beat = None;
                            cx.notify();
                        }) {
                            tracing::debug!(%error, "bot face released before its wink ended");
                        }
                    })
                    .detach();
                }),
            );
        moving_face(face, self.size, &course)
    }
}
