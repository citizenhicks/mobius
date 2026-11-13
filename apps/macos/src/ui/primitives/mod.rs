//! The shared design language: tokens, type, controls, lists, sidebar, feedback and dialogs.

mod bot_face;
mod controls;
mod dialog;
mod feedback;
mod format;
mod glyphs;
#[cfg(test)]
mod guard;
mod list;
pub(crate) mod motion;
mod navigation;
pub(crate) mod organic;
mod scrolling_title;
mod shape;
mod shimmer;
mod spinner;
mod text;
mod tokens;

#[cfg(feature = "snapshot")]
pub(crate) use bot_face::glide_parity;
pub(crate) use bot_face::{BotFace, BotFaceView, BotMood, lively_face};
pub(crate) use controls::*;
pub(crate) use dialog::{EditKind, TextEdit, dialog_actions, dialog_body, labelled};
pub(crate) use feedback::*;
pub(crate) use format::*;
pub(crate) use glyphs::*;
pub(crate) use list::*;
pub(crate) use navigation::*;
pub(crate) use shape::{Shape, pill_quad};
pub(crate) use shimmer::Shimmer;
pub(crate) use spinner::Spinner;
pub(crate) use text::*;
pub(crate) use tokens::{alpha, face, glyph, height, inset, radius, space, width};
