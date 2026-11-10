//! The sandbox middleware manifest and its configured limits.

use crate::Error;
use crate::Result;
use crate::middleware::manifest::MiddlewareManifest;

/// Configuration and presentation metadata for sandbox approval policy.
pub static MANIFEST: std::sync::LazyLock<MiddlewareManifest> =
    std::sync::LazyLock::new(|| MiddlewareManifest {
        id: "sandbox",
        label: super::text::DEFINITION.manifest_label.as_str(),
        description: super::text::DEFINITION.manifest_description.as_str(),
        required: true,
        default_enabled: true,
        settings: &super::text::DEFINITION.settings,
        requires: &[],
    });

pub(super) const MAX_TOOL_OUTPUT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_BACKGROUND_COMMANDS: usize = 256;

/// Default foreground shell command deadline in seconds.
#[must_use]
pub fn default_command_timeout_seconds() -> u64 {
    super::text::DEFINITION.defaults_command_timeout_seconds
}

/// Default retained text budget for command execution and tool dispatch.
#[must_use]
pub fn default_tool_output_limit() -> usize {
    super::text::DEFINITION.defaults_tool_output_bytes
}

/// Default number of tracked background commands per execution boundary.
#[must_use]
pub fn default_background_command_limit() -> usize {
    super::text::DEFINITION.defaults_background_commands
}

/// A retained tool-output budget in UTF-8 bytes, checked against the internal safety bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputLimit(pub(super) usize);

impl OutputLimit {
    /// Checks a budget once; sandbox setters then take it as is.
    /// # Errors
    /// Returns an error for a zero budget or one above the internal safety bound.
    pub fn new(bytes: usize) -> Result<Self> {
        if !(1..=MAX_TOOL_OUTPUT_BYTES).contains(&bytes) {
            return Err(Error::Config(format!(
                "tool output limit must be between 1 and {MAX_TOOL_OUTPUT_BYTES} bytes"
            )));
        }
        Ok(Self(bytes))
    }

    /// Returns the budget in bytes.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for OutputLimit {
    fn default() -> Self {
        Self(default_tool_output_limit())
    }
}
