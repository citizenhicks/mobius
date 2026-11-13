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
mod pose;
mod view;

#[cfg(feature = "snapshot")]
pub(crate) use draw::glide_parity;
pub(crate) use view::{BotFaceView, lively_face};

use super::controls::palette;
use super::motion;
use super::{FontSet, Shape};
use crate::numeric::ToFloat;
pub(crate) use draw::{BotFace, shape_label};
use draw::{COG_SPRING, COG_TICK, DETAIL, Eyes, Z_GAP, Z_LIFE, clock, face, moving_face};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Div, Entity, FontWeight, Hsla, Motion, MotionExt as _, MotionFrame,
    MouseButton, Pixels, Point, SharedString, Subscription, Task, Transformation, Window, black,
    div, point, px, radians, relative, rgba, svg, white,
};
use gpui_component::menu::PopupMenuItem;
use mobius_gateway::wire::BotShape;
pub(crate) use pose::BotMood;
use pose::{Course, Glide, Live, MOOD_SPRING, POSE_SPRING, Pose, between, cog_turn};
use std::cell::Cell;
use std::hash::{BuildHasher, Hasher};
use std::rc::Rc;
use std::time::{Duration, Instant};
