//! Bots: the list, each Bot's overview and settings, routines and agent defaults (WP4).

mod bots_ui;
mod capabilities;
mod capability_rows;
mod create;
mod events;
mod menu;
mod model;
mod overview;
mod pin;
mod routine_dialog;
mod routine_form;
mod routines;
mod runs;
mod schedule;
mod sections;
mod settings;
mod sidebar;
mod status;

pub(in crate::ui) use capabilities::extension_metadata;
pub(in crate::ui) use create::dialog_heading;
pub(in crate::ui) use settings::AgentScope;

pub(in crate::ui) use bots_ui::{BotsUi, Pending};
