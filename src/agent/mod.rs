//! Agent handles and the single linear command dispatch loop.

mod approval;
mod config;
mod context_estimate;
mod handle;
mod input;
mod recorder;
mod recovery;
mod runner;
mod sender;
mod startup;
mod tool_step;
mod turn;

pub use self::config::*;
pub use self::handle::*;
pub use self::runner::*;
pub use self::sender::*;
pub use self::startup::create_agent;

use self::recorder::EventRecorder;

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
