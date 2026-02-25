use crate::KeyValue;
use core::fmt;
use std::sync::Arc;

use super::{BoundSyncInstrument, SyncInstrument};

/// An instrument that records increasing values.
///
/// [`Counter`] can be cloned to create multiple handles to the same instrument. If a [`Counter`] needs to be shared,
/// users are recommended to clone the [`Counter`] instead of creating duplicate [`Counter`]s for the same metric. Creating
/// duplicate [`Counter`]s for the same metric could lower SDK performance.
#[derive(Clone)]
#[non_exhaustive]
pub struct Counter<T>(Arc<dyn SyncInstrument<T> + Send + Sync>);

impl<T> fmt::Debug for Counter<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("Counter<{}>", std::any::type_name::<T>()))
    }
}

impl<T> Counter<T> {
    /// Create a new counter.
    pub fn new(inner: Arc<dyn SyncInstrument<T> + Send + Sync>) -> Self {
        Counter(inner)
    }

    /// Records an increment to the counter.
    pub fn add(&self, value: T, attributes: &[KeyValue]) {
        self.0.measure(value, attributes)
    }

    /// Create a pre-bound counter handle for the given attribute set.
    ///
    /// The returned [`BoundCounter`] caches a direct reference to the underlying
    /// aggregator, bypassing per-call attribute sort, hash, and lookup.
    /// This is significantly faster for hot paths that always record with
    /// the same attributes.
    pub fn bind(&self, attributes: &[KeyValue]) -> BoundCounter<T> {
        BoundCounter(self.0.bind(attributes))
    }
}

/// A pre-bound counter handle that records increments without per-call attribute lookup.
///
/// Created by [`Counter::bind`]. Holds a cached reference to the underlying aggregator.
pub struct BoundCounter<T>(Box<dyn BoundSyncInstrument<T>>);

impl<T> fmt::Debug for BoundCounter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "BoundCounter<{}>",
            std::any::type_name::<T>()
        ))
    }
}

impl<T> BoundCounter<T> {
    /// Records an increment using the pre-bound attributes.
    pub fn add(&self, value: T) {
        self.0.measure(value)
    }
}

/// An async instrument that records increasing values.
#[derive(Clone)]
#[non_exhaustive]
pub struct ObservableCounter<T> {
    _marker: std::marker::PhantomData<T>,
}

impl<T> ObservableCounter<T> {
    /// Create a new observable counter.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        ObservableCounter {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T> fmt::Debug for ObservableCounter<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "ObservableCounter<{}>",
            std::any::type_name::<T>()
        ))
    }
}
