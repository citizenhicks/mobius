//! Shared motion durations and presentation rates.

use std::time::Duration;

// Shared timing tiers; named durations below keep call sites readable by purpose.
/// Glyph swaps, carets and small marks changing state.
pub const QUICK: Duration = Duration::from_millis(180);
/// Layout: rows arriving, panels, toasts, sections opening.
pub const STANDARD: Duration = Duration::from_millis(280);
/// A new transcript row settling in.
pub const REVEAL: Duration = STANDARD;
/// The sidebar and the side pane sliding open or closed.
pub(super) const PANEL: Duration = QUICK;
/// A titlebar control appearing.
pub const APPEAR: Duration = QUICK;
pub const CROSSFADE: Duration = STANDARD;
pub const BLINK: Duration = Duration::from_millis(110);
pub const WINK: Duration = Duration::from_millis(800);
pub const REACTION: Duration = Duration::from_millis(1400);
pub const SHIMMER_PERIOD: Duration = Duration::from_millis(1600);
/// At most 30 frames per second.
pub const FRAME: Duration = Duration::from_millis(33);
/// A display frame, for motion driven from a task rather than the renderer.
pub const FRAME_60: Duration = Duration::from_millis(16);

/// GPU motion that should read as smooth movement: glides, sweeps, spins and reveals.
pub const SMOOTH_FPS: f32 = 60.;
/// GPU motion in the background of a scene: pulses, ticking cogs and drifting z's.
pub const AMBIENT_FPS: f32 = 30.;
/// The shared loading spinner makes one turn.
pub const SPINNER_PERIOD: Duration = Duration::from_millis(900);
