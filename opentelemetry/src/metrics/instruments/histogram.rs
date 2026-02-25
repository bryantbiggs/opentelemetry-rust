use crate::KeyValue;
use core::fmt;
use std::sync::Arc;

use super::{BoundSyncInstrument, SyncInstrument};

/// An instrument that records a distribution of values.
///
/// [`Histogram`] can be cloned to create multiple handles to the same instrument. If a [`Histogram`] needs to be shared,
/// users are recommended to clone the [`Histogram`] instead of creating duplicate [`Histogram`]s for the same metric. Creating
/// duplicate [`Histogram`]s for the same metric could lower SDK performance.
#[derive(Clone)]
#[non_exhaustive]
pub struct Histogram<T>(Arc<dyn SyncInstrument<T> + Send + Sync>);

impl<T> fmt::Debug for Histogram<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("Histogram<{}>", std::any::type_name::<T>()))
    }
}

impl<T> Histogram<T> {
    /// Create a new histogram.
    pub fn new(inner: Arc<dyn SyncInstrument<T> + Send + Sync>) -> Self {
        Histogram(inner)
    }

    /// Adds an additional value to the distribution.
    pub fn record(&self, value: T, attributes: &[KeyValue]) {
        self.0.measure(value, attributes)
    }

    /// Create a pre-bound histogram handle for the given attribute set.
    ///
    /// The returned [`BoundHistogram`] caches a direct reference to the underlying
    /// aggregator, bypassing per-call attribute sort, hash, and lookup.
    pub fn bind(&self, attributes: &[KeyValue]) -> BoundHistogram<T> {
        BoundHistogram(self.0.bind(attributes))
    }
}

/// A pre-bound histogram handle that records values without per-call attribute lookup.
///
/// Created by [`Histogram::bind`]. Holds a cached reference to the underlying aggregator.
pub struct BoundHistogram<T>(Box<dyn BoundSyncInstrument<T>>);

impl<T> fmt::Debug for BoundHistogram<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!(
            "BoundHistogram<{}>",
            std::any::type_name::<T>()
        ))
    }
}

impl<T> BoundHistogram<T> {
    /// Records a value using the pre-bound attributes.
    pub fn record(&self, value: T) {
        self.0.measure(value)
    }
}
