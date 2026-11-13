//! SwiftUI's BotFace: a tinted silhouette whose eyes carry the Bot's mood.
//!
//! `BotFace` is the static face for lists and rows. `lively_face` returns a
//! `BotFaceView` entity for hero and header faces: it animates only while it is
//! rendered. Glides between poses, ticking cogs and drifting z's are GPU motions, so
//! the window renders again only when a beat picks a new pose, a blink starts or ends,
//! or the mood changes. Faces of at least `DETAIL` size also think with cogs for eyes
//! and sleep with drifting z's.
//!
//! Every face is lit by two fixed SVG layers over its silhouette, a light from the
//! top left and a shaded rim, so depth costs no extra frames. Faces of `DETAIL`
//! size also get brows and blush that follow the mood, glints in round eyes, and a
//! ground shadow that shrinks as the head lifts.

mod draw;
#[cfg(feature = "snapshot")]
mod glide_parity;
mod mood;
mod pose;
mod view;

pub(crate) use draw::{BotFace, shape_label};
#[cfg(feature = "snapshot")]
pub(crate) use glide_parity::glide_parity;
pub(crate) use mood::BotMood;
pub(crate) use view::{BotFaceView, lively_face};
