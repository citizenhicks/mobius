//! Bounded workspace instruction discovery.

use std::io::Read as _;
use std::path::Path;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
#[cfg(unix)]
use cap_std::fs::MetadataExt as _;

use super::Middleware;
use super::PromptSection;
use super::manifest::MiddlewareManifest;
use crate::{Error, Result};

mod text {
    include!(concat!(
        env!("OUT_DIR"),
        "/src_middleware_instructions_text.rs"
    ));
}

const OVERRIDE_FILE: &str = "AGENTS.override.md";
const INSTRUCTIONS_FILE: &str = "AGENTS.md";
const MAX_INSTRUCTIONS_BYTES: u64 = 40_000;

/// Configuration and presentation metadata for workspace instructions.
pub const MANIFEST: MiddlewareManifest = MiddlewareManifest {
    id: "instructions",
    label: text::MANIFEST_LABEL,
    description: text::MANIFEST_DESCRIPTION,
    required: false,
    default_enabled: false,
    settings: &[],
};

/// Optional root workspace instructions composed into the system prompt once.
pub struct Instructions {
    body: Option<String>,
}

impl Instructions {
    /// Loads `AGENTS.override.md`, falling back to `AGENTS.md` when absent.
    pub fn discover(workspace: impl AsRef<Path>) -> Result<Self> {
        let workspace = Dir::open_ambient_dir(workspace, ambient_authority())?;
        for name in [OVERRIDE_FILE, INSTRUCTIONS_FILE] {
            let Some(content) = read_optional(&workspace, name)? else {
                continue;
            };
            let content = content.trim();
            return Ok(Self {
                body: (!content.is_empty()).then(|| format!("Source: `{name}`\n\n{content}")),
            });
        }
        Ok(Self { body: None })
    }

    fn section(&self) -> Option<PromptSection> {
        self.body
            .clone()
            .map(|body| PromptSection::titled(text::PROMPT_TITLE, body))
    }
}

impl Middleware for Instructions {
    fn name(&self) -> &'static str {
        MANIFEST.id
    }

    fn prompt_section(&self, _runtime: &super::RuntimeContext) -> Result<Option<PromptSection>> {
        Ok(self.section())
    }
}

fn read_optional(directory: &Dir, name: &str) -> Result<Option<String>> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_symlink() || !metadata.is_file() {
        return Err(Error::Config(format!(
            "workspace instruction `{name}` must be a regular file"
        )));
    }
    let file = directory.open(name)?;
    let opened = file.metadata()?;
    let current = directory.symlink_metadata(name)?;
    if !opened.is_file()
        || current.is_symlink()
        || !same_file(&metadata, &opened)
        || !same_file(&opened, &current)
    {
        return Err(Error::Config(format!(
            "workspace instruction `{name}` changed while opening"
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_INSTRUCTIONS_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INSTRUCTIONS_BYTES {
        return Err(Error::Config(format!(
            "workspace instruction `{name}` exceeds {MAX_INSTRUCTIONS_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| Error::Config(format!("workspace instruction `{name}` is not valid UTF-8")))
}

#[cfg(unix)]
fn same_file(left: &cap_std::fs::Metadata, right: &cap_std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(_left: &cap_std::fs::Metadata, _right: &cap_std::fs::Metadata) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_and_missing_files_are_optional() {
        let workspace = tempfile::tempdir().expect("workspace");
        assert!(
            Instructions::discover(workspace.path())
                .expect("empty workspace")
                .body
                .is_none()
        );
        std::fs::write(workspace.path().join(INSTRUCTIONS_FILE), "base").expect("base");
        std::fs::write(workspace.path().join(OVERRIDE_FILE), "override").expect("override");

        let instructions = Instructions::discover(workspace.path()).expect("instructions");

        assert_eq!(
            instructions.section(),
            Some(PromptSection::titled(
                "workspace instructions",
                "Source: `AGENTS.override.md`\n\noverride"
            ))
        );
    }

    #[cfg(unix)]
    #[test]
    fn instruction_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().expect("workspace");
        let outside = tempfile::NamedTempFile::new().expect("outside");
        symlink(outside.path(), workspace.path().join(INSTRUCTIONS_FILE)).expect("symlink");

        assert!(Instructions::discover(workspace.path()).is_err());
    }
}
