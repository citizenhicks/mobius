//! Development-only visual capture: `MOBIUS_SNAPSHOT=<dir> cargo run --features snapshot`.
//! Walks each package's scenarios against the live gateway and writes one PNG per screen.
//! `MOBIUS_SNAPSHOT_ONLY=a,b` limits scenarios; `MOBIUS_SNAPSHOT_SESSION=<id>` picks the chat;
//! `MOBIUS_SNAPSHOT_APPEARANCE=light|dark` forces the appearance without saving it;
//! `MOBIUS_SNAPSHOT_HOLD_MS=<ms>` keeps each screen up that long after its capture, for
//! `screencapture` of native views or for driving the in-app browser from outside.
//! `MOBIUS_SNAPSHOT_REDUCE_MOTION=1` disables motion for this capture process only.
//! Native QuickLook and terminal panels are not part of the GPUI scene and render blank.
//! Scenarios only change local UI state; they never send chat messages or edit gateway records.
#![expect(
    clippy::print_stderr,
    reason = "this development harness reports each capture and check on stderr"
)]

mod attachments;
mod background;
mod bots;
mod capture;
mod chat;
mod config;
mod content;
mod documents;
mod harness;
mod memory;
mod probes;
mod selection;
mod shell;
mod sidebar;

use crate::ui::Workspace;
use gpui::{Context, Window};

pub(super) type Step = fn(&mut Workspace, &mut Window, &mut Context<Workspace>);
pub(super) type Scenario = (&'static str, Step);

/// Scenarios that click their probed element after the first capture, twice, drawing
/// frame by frame so a transient layout jump shows in the probe log and the captures.
const CLICKS: [&[&str]; 1] = [chat::CLICKS];

const PACKAGES: [&[Scenario]; 12] = [
    shell::SCENARIOS,
    sidebar::SCENARIOS,
    chat::SCENARIOS,
    attachments::SCENARIOS,
    content::SCENARIOS,
    bots::SCENARIOS,
    config::SCENARIOS,
    selection::SCENARIOS,
    background::SCENARIOS,
    documents::SCENARIOS,
    harness::SCENARIOS,
    memory::SCENARIOS,
];

pub(in crate::ui) use capture::reduce_motion;
pub(in crate::ui) use probes::{activity, count_render, probe, probed, track, watch};
pub(super) use probes::{strip_after_capture, undo_after_capture};
