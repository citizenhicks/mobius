//! JSON encoding that stops at a byte budget, shared by size checks and bounded encoders.

use std::io;
use std::io::Write;

use serde::Serialize;

/// What [`write_within`] produced when the encoding fit its budget.
#[derive(Debug)]
#[non_exhaustive]
pub struct Written<W> {
    /// The writer, holding everything written to it.
    pub writer: W,
    /// The bytes this encoding added.
    pub len: usize,
}

/// Encodes `value` as JSON into `writer`, stopping as soon as it would pass `limit` bytes.
///
/// Returns `None` when the encoding exceeds `limit`; whatever was written before the stop
/// stays in the writer, which the caller then discards.
///
/// # Errors
///
/// Returns the serializer's or the writer's own error.
pub fn write_within<W: Write, T: Serialize + ?Sized>(
    writer: W,
    value: &T,
    limit: usize,
) -> serde_json::Result<Option<Written<W>>> {
    let mut bounded = Bounded {
        inner: writer,
        len: 0,
        limit,
        exceeded: false,
    };
    match serde_json::to_writer(&mut bounded, value) {
        Ok(()) => Ok(Some(Written {
            writer: bounded.inner,
            len: bounded.len,
        })),
        Err(_) if bounded.exceeded => Ok(None),
        Err(error) => Err(error),
    }
}

/// The serialized JSON length of `value`, or `None` when it exceeds `limit`.
pub(crate) fn len_within<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
) -> serde_json::Result<Option<usize>> {
    Ok(write_within(io::sink(), value, limit)?.map(|written| written.len))
}

/// The serialized JSON length of `value`.
pub(crate) fn serialized_len<T: Serialize + ?Sized>(value: &T) -> crate::Result<usize> {
    len_within(value, usize::MAX)?
        .ok_or_else(|| crate::Error::Internal("serialized size overflow".into()))
}

struct Bounded<W> {
    inner: W,
    len: usize,
    limit: usize,
    exceeded: bool,
}

impl<W: Write> Write for Bounded<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let len = self.len.saturating_add(buffer.len());
        if len > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("encoding exceeded its byte budget"));
        }
        self.inner.write_all(buffer)?;
        self.len = len;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stops_at_the_budget_and_keeps_what_fits() {
        let value = serde_json::json!({"text": "hello"});
        let fit = write_within(Vec::new(), &value, 16).unwrap().unwrap();
        assert_eq!(fit.writer, br#"{"text":"hello"}"#);
        assert_eq!(fit.len, 16);
        assert!(write_within(Vec::new(), &value, 15).unwrap().is_none());
    }
}
