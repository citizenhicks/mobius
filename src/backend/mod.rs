//! Runtime adapters used by the agent loop.

pub mod checkpoint;
pub mod model;
pub mod sandbox;

/// Immutable session files shared by observations, uploads, and artifacts.
pub mod session_files;

/// Bounds one read against the recorded file size and returns the next offset, if any.
fn chunk_bounds(size: u64, offset: u64, limit: usize) -> Option<(usize, Option<u64>)> {
    let remaining = size.checked_sub(offset)?;
    let length = usize::try_from(remaining).unwrap_or(usize::MAX).min(limit);
    let end = offset + length as u64;
    Some((length, (end < size).then_some(end)))
}
