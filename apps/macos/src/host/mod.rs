//! macOS: the native bridge to Swift, embedded Chromium and the agent's browser endpoint.
//! The gateway core never reaches in here; the host hands it what it needs at startup.

pub mod agent_browser;
pub mod browser;
pub mod http;
pub mod platform;
