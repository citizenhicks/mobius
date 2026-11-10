//! Sandboxed execution and its approval boundary.

mod approval;
mod backend;
mod background;
mod command_output;
pub mod local;
mod manifest;
mod permissions;
mod process_group;
mod runtime;
mod sandbox_mode;
mod text;
mod worker;
pub use worker::{WorkerCommand, WorkerProcess};

pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_BINARY_FILE_BYTES: usize = 50 * 1024 * 1024;

pub use approval::ApprovalPolicy;
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub use process_group::MACOS_COMMAND_WRAPPER;
#[doc(hidden)]
pub use process_group::ProcessGroupGuard;

pub use backend::*;
pub(crate) use background::BackgroundCommandPoll;
pub use command_output::*;
pub use manifest::*;
pub use permissions::*;
pub use runtime::*;
pub use sandbox_mode::*;
