//! Runtime adapters used by the agent loop.

pub mod checkpoint;
pub mod model;
pub mod sandbox;

/// Immutable session files shared by observations, uploads, and artifacts.
pub mod session_files;
