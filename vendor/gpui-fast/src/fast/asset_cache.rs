use crate::asset_cache::{CachedLoad, hash};
use crate::{App, Asset};
use std::any::TypeId;

impl App {
    /// Remove an asset from the cache and return its completed output, without loading it.
    /// A pending load is cancelled; an absent asset stays absent.
    pub fn take_asset<A: Asset>(&mut self, source: &A::Source) -> Option<A::Output> {
        let entry = self
            .loading_assets
            .remove(&(TypeId::of::<A>(), hash(source)))?;
        entry
            .downcast_ref::<CachedLoad<A::Output>>()
            .expect("asset cache entries are keyed by their asset type")
            .get()
    }
}
