//! GPU presentation motion and layout animation share timing and the snapshot clock.
//! Layout-driven widths/heights still render. Title marquees, digit counters and memory
//! graph edges deliberately remain on the layout path for 0.17.8.

mod appear;
mod clock;
mod counter;
mod height;
mod reveal;
mod slide;
pub mod spring;
mod timing;

pub use appear::{fade_in, rise_in};
#[cfg(feature = "snapshot")]
pub use clock::set_clock;
pub use clock::{in_view, is_animating, now, sampled, seconds};
pub use counter::counter;
pub use height::{hand_off, progress, smooth, visible};
pub use reveal::{Reveal, Reveals};
pub use slide::slide;
pub use timing::{
    AMBIENT_FPS, APPEAR, BLINK, CROSSFADE, FRAME, FRAME_60, QUICK, REACTION, REVEAL,
    SHIMMER_PERIOD, SMOOTH_FPS, SPINNER_PERIOD, STANDARD, WINK,
};
