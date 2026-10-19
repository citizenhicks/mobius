//! Borrowed logical request input without copying durable history.

use std::ops::Index;
use std::sync::Arc;

use serde::Serialize;
use serde::ser::SerializeSeq as _;
use serde_json::Value;

use crate::{Error, Result};

#[derive(Debug, Clone, Copy)]
enum Source<'a> {
    Values(&'a [Value]),
    Shared(&'a [Arc<Value>]),
}

impl<'a> Source<'a> {
    fn len(self) -> usize {
        match self {
            Self::Values(items) => items.len(),
            Self::Shared(items) => items.len(),
        }
    }

    fn get(self, index: usize) -> Option<&'a Value> {
        match self {
            Self::Values(items) => items.get(index),
            Self::Shared(items) => items.get(index).map(Arc::as_ref),
        }
    }

    fn prefix(self, end: usize) -> Self {
        match self {
            Self::Values(items) => Self::Values(items.get(..end).unwrap_or(items)),
            Self::Shared(items) => Self::Shared(items.get(..end).unwrap_or(items)),
        }
    }

    fn suffix(self, start: usize) -> Self {
        match self {
            Self::Values(items) => Self::Values(items.get(start..).unwrap_or_default()),
            Self::Shared(items) => Self::Shared(items.get(start..).unwrap_or_default()),
        }
    }
}

/// Borrowed committed history, staged input and optional request-only guidance.
/// Serializes as one ordered JSON array; no source items are cloned.
#[derive(Debug, Clone, Copy)]
pub struct ModelInput<'a> {
    sources: [Source<'a>; 3],
}

impl<'a> ModelInput<'a> {
    /// Reads shared committed history followed by the staged tail.
    #[must_use]
    pub(crate) fn shared_parts(prefix: &'a [Arc<Value>], tail: &'a [Arc<Value>]) -> Self {
        Self {
            sources: [
                Source::Shared(prefix),
                Source::Shared(tail),
                Source::Values(&[]),
            ],
        }
    }

    /// Appends a borrowed source, preserving the existing prefix.
    /// # Errors
    /// Returns an error if the combined view needs more than three nonempty slices.
    pub(crate) fn with_appended(self, appended: Self) -> Result<Self> {
        let mut sources = [Source::Values(&[]); 3];
        let mut count = 0;
        for source in self.sources.into_iter().chain(appended.sources) {
            if source.len() == 0 {
                continue;
            }
            let destination = sources.get_mut(count).ok_or_else(|| {
                Error::Config("model input supports at most three borrowed slices".into())
            })?;
            *destination = source;
            count += 1;
        }
        Ok(Self { sources })
    }

    /// Number of logical input items.
    #[must_use]
    pub fn len(self) -> usize {
        self.sources.into_iter().map(Source::len).sum()
    }

    /// Whether the logical input contains no items.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Borrows an item at its logical position.
    #[must_use]
    pub fn get(self, mut index: usize) -> Option<&'a Value> {
        for source in self.sources {
            if index < source.len() {
                return source.get(index);
            }
            index -= source.len();
        }
        None
    }

    /// Borrows the shared handle when this position comes from durable history.
    #[must_use]
    pub(crate) fn shared_item(self, mut index: usize) -> Option<&'a Arc<Value>> {
        for source in self.sources {
            if index < source.len() {
                return match source {
                    Source::Shared(items) => items.get(index),
                    Source::Values(_) => None,
                };
            }
            index -= source.len();
        }
        None
    }

    /// Iterates over the logical sequence while borrowing every item.
    pub fn iter(self) -> impl ExactSizeIterator<Item = &'a Value> + DoubleEndedIterator + Clone {
        Items(self)
    }

    /// Takes at most the first `end` items without copying them.
    #[must_use]
    pub fn prefix(mut self, mut end: usize) -> Self {
        for source in &mut self.sources {
            let retained = end.min(source.len());
            *source = source.prefix(retained);
            end -= retained;
        }
        self
    }

    /// Skips at most the first `start` items without copying them.
    #[must_use]
    pub fn suffix(mut self, mut start: usize) -> Self {
        for source in &mut self.sources {
            let skipped = start.min(source.len());
            *source = source.suffix(skipped);
            start -= skipped;
        }
        self
    }
}

impl<'a> From<&'a [Value]> for ModelInput<'a> {
    fn from(items: &'a [Value]) -> Self {
        Self {
            sources: [
                Source::Values(items),
                Source::Values(&[]),
                Source::Values(&[]),
            ],
        }
    }
}

impl<'a, const N: usize> From<&'a [Value; N]> for ModelInput<'a> {
    fn from(items: &'a [Value; N]) -> Self {
        items.as_slice().into()
    }
}

impl<'a> From<&'a Vec<Value>> for ModelInput<'a> {
    fn from(items: &'a Vec<Value>) -> Self {
        items.as_slice().into()
    }
}

impl<'a> From<&'a [Arc<Value>]> for ModelInput<'a> {
    fn from(items: &'a [Arc<Value>]) -> Self {
        Self::shared_parts(items, &[])
    }
}

impl<'a> From<&'a Vec<Arc<Value>>> for ModelInput<'a> {
    fn from(items: &'a Vec<Arc<Value>>) -> Self {
        items.as_slice().into()
    }
}

#[derive(Clone)]
struct Items<'a>(ModelInput<'a>);

impl<'a> Iterator for Items<'a> {
    type Item = &'a Value;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.0.get(0)?;
        self.0 = self.0.suffix(1);
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.0.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for Items<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let last = self.0.len().checked_sub(1)?;
        let item = self.0.get(last)?;
        self.0 = self.0.prefix(last);
        Some(item)
    }
}

impl ExactSizeIterator for Items<'_> {}

impl Index<usize> for ModelInput<'_> {
    type Output = Value;

    #[expect(
        clippy::expect_used,
        reason = "Index panics on an out-of-bounds position, like slice indexing; get is the checked form"
    )]
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("model input index is in bounds")
    }
}

impl Serialize for ModelInput<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for item in self.iter() {
            sequence.serialize_element(item)?;
        }
        sequence.end()
    }
}
