mod aggregate;
mod exponential_histogram;
mod histogram;
mod last_value;
mod precomputed_sum;
mod sum;

use core::fmt;
#[cfg(not(target_has_atomic = "64"))]
use portable_atomic::{AtomicI64, AtomicU64};
use std::cmp::min;
use std::collections::HashMap;
use std::ops::{Add, AddAssign, Sub};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::sync::OnceLock;

pub(crate) use aggregate::{AggregateBuilder, AggregateFns, ComputeAggregation, Measure};
pub(crate) use exponential_histogram::{EXPO_MAX_SCALE, EXPO_MIN_SCALE};
use opentelemetry::KeyValue;

use super::data::{AggregatedMetrics, MetricData};
use super::pipeline::DEFAULT_CARDINALITY_LIMIT;

// TODO Replace it with LazyLock once it is stable
pub(crate) static STREAM_OVERFLOW_ATTRIBUTES: OnceLock<Vec<KeyValue>> = OnceLock::new();

#[inline]
fn stream_overflow_attributes() -> &'static Vec<KeyValue> {
    STREAM_OVERFLOW_ATTRIBUTES.get_or_init(|| vec![KeyValue::new("otel.metric.overflow", true)])
}

use core::hash::{BuildHasher, Hasher};

/// Pre-computed hash of a sorted+deduped attribute set.
/// The hash is computed once via foldhash when the attributes are first seen,
/// then carried through all subsequent lookups via PassthroughHasher.
#[derive(Clone, Debug)]
struct HashedAttributes {
    /// Sorted, deduplicated attributes (canonical form).
    attrs: Vec<KeyValue>,
    /// Pre-computed foldhash of the canonical attributes.
    hash: u64,
}

impl HashedAttributes {
    /// Create from raw attributes: sort, dedup, hash once.
    fn from_raw(attributes: &[KeyValue]) -> Self {
        let mut sorted = attributes.to_vec();
        sorted.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        sorted.dedup_by(|a, b| a.key == b.key);
        let hash = Self::compute_hash(&sorted);
        HashedAttributes {
            attrs: sorted,
            hash,
        }
    }

    /// Create from already-sorted attributes (e.g., overflow sentinel).
    fn from_sorted(attrs: Vec<KeyValue>) -> Self {
        let hash = Self::compute_hash(&attrs);
        HashedAttributes { attrs, hash }
    }

    fn compute_hash(attrs: &[KeyValue]) -> u64 {
        use std::hash::Hash;
        let state = foldhash::fast::FixedState::with_seed(0);
        let mut hasher = state.build_hasher();
        attrs.hash(&mut hasher);
        hasher.finish()
    }
}

impl PartialEq for HashedAttributes {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.attrs == other.attrs
    }
}

impl Eq for HashedAttributes {}

impl std::hash::Hash for HashedAttributes {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Write pre-computed hash directly — PassthroughHasher will use this
        state.write_u64(self.hash);
    }
}

/// A hasher that returns the pre-computed hash value directly.
/// Used with HashedAttributes to avoid re-hashing inside HashMap.
struct PassthroughHasher(u64);

impl Hasher for PassthroughHasher {
    fn write(&mut self, _bytes: &[u8]) {
        // Should not be called — HashedAttributes::hash writes u64 directly
        unreachable!("PassthroughHasher only supports write_u64");
    }

    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

#[derive(Clone, Default)]
struct PassthroughBuildHasher;

impl BuildHasher for PassthroughBuildHasher {
    type Hasher = PassthroughHasher;

    fn build_hasher(&self) -> PassthroughHasher {
        PassthroughHasher(0)
    }
}

pub(crate) trait Aggregator {
    /// A static configuration that is needed in order to initialize aggregator.
    /// E.g. bucket_size at creation time .
    type InitConfig;

    /// Some aggregators can do some computations before updating aggregator.
    /// This helps to reduce contention for aggregators because it makes
    /// [`Aggregator::update`] as short as possible.
    type PreComputedValue;

    /// Called everytime a new attribute-set is stored.
    fn create(init: &Self::InitConfig) -> Self;

    /// Called for each measurement.
    fn update(&self, value: Self::PreComputedValue);

    /// Return current value and reset this instance
    fn clone_and_reset(&self, init: &Self::InitConfig) -> Self;
}

/// Wraps an aggregator with a status flag for delta collection optimization.
/// The `has_been_updated` flag tracks whether the tracker has received measurements
/// since the last collection, enabling in-place iteration during delta collect
/// instead of draining the entire map.
struct TrackerEntry<A> {
    aggregator: A,
    has_been_updated: AtomicBool,
}

impl<A: Aggregator> TrackerEntry<A> {
    fn new(aggregator: A) -> Self {
        TrackerEntry {
            aggregator,
            has_been_updated: AtomicBool::new(true),
        }
    }
}

use std::sync::Mutex;

/// Number of shards — must be a power of two for fast modulo via bitmask.
const NUM_SHARDS: usize = 16;

type ShardHashMap<A> = HashMap<HashedAttributes, TrackerEntry<A>, PassthroughBuildHasher>;

/// A sharded concurrent map for attribute-to-tracker lookup.
/// Each shard is independently locked, reducing contention on measure() from
/// N threads competing for one RwLock to N/NUM_SHARDS threads per Mutex.
struct ShardedMap<A: Aggregator> {
    shards: Box<[Mutex<ShardHashMap<A>>; NUM_SHARDS]>,
}

impl<A: Aggregator> ShardedMap<A> {
    fn new(capacity_per_shard: usize) -> Self {
        ShardedMap {
            shards: Box::new(std::array::from_fn(|_| {
                Mutex::new(HashMap::with_capacity_and_hasher(
                    capacity_per_shard,
                    PassthroughBuildHasher,
                ))
            })),
        }
    }

    /// Select shard using the pre-computed hash.
    #[inline]
    fn shard_index(hash: u64) -> usize {
        (hash as usize) & (NUM_SHARDS - 1)
    }

    /// Look up or insert a tracker for the given attributes.
    /// If cardinality limit is reached, routes measurement to the overflow bucket.
    #[inline]
    fn measure(
        &self,
        hashed_attrs: &HashedAttributes,
        value: A::PreComputedValue,
        config: &A::InitConfig,
        count: &AtomicUsize,
        cardinality_limit: usize,
        overflow_attrs: &HashedAttributes,
    ) {
        let idx = Self::shard_index(hashed_attrs.hash);
        let mut shard = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());

        if let Some(entry) = shard.get(hashed_attrs) {
            entry.aggregator.update(value);
            entry.has_been_updated.store(true, Ordering::Relaxed);
            return;
        }

        // Not found — check cardinality limit before inserting
        if count.load(Ordering::SeqCst) < cardinality_limit {
            let new_entry = TrackerEntry::new(A::create(config));
            new_entry.aggregator.update(value);
            shard.insert(hashed_attrs.clone(), new_entry);
            count.fetch_add(1, Ordering::SeqCst);
            return;
        }

        // Over cardinality limit — route to overflow bucket.
        // Drop current shard lock first to avoid potential deadlock if overflow
        // lands in the same shard.
        drop(shard);

        let overflow_idx = Self::shard_index(overflow_attrs.hash);
        let mut overflow_shard = self.shards[overflow_idx]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = overflow_shard.get(overflow_attrs) {
            entry.aggregator.update(value);
            entry.has_been_updated.store(true, Ordering::Relaxed);
        } else {
            let entry = TrackerEntry::new(A::create(config));
            entry.aggregator.update(value);
            overflow_shard.insert(overflow_attrs.clone(), entry);
        }
    }

    /// Iterate all shards, calling `f` for each entry.
    /// Locks one shard at a time to minimize contention with measure().
    fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(&HashedAttributes, &TrackerEntry<A>),
    {
        for shard in self.shards.iter() {
            let shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            for (attrs, entry) in shard.iter() {
                f(attrs, entry);
            }
        }
    }

    /// Iterate all shards, retaining only entries where `f` returns true.
    /// Used for stale entry eviction in delta collect.
    fn retain<F>(&self, mut f: F)
    where
        F: FnMut(&HashedAttributes, &TrackerEntry<A>) -> bool,
    {
        for shard in self.shards.iter() {
            let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            shard.retain(|attrs, entry| f(attrs, entry));
        }
    }

    /// Drain all shards, returning all entries. Used by drain_and_reset.
    fn drain_all(&self) -> Vec<(HashedAttributes, TrackerEntry<A>)> {
        let mut result = Vec::new();
        for shard in self.shards.iter() {
            let mut shard = shard.lock().unwrap_or_else(|e| e.into_inner());
            result.extend(shard.drain());
        }
        result
    }
}

/// The storage for sums.
///
/// This structure is parametrized by an `Operation` that indicates how
/// updates to the underlying value trackers should be performed.
pub(crate) struct ValueMap<A>
where
    A: Aggregator,
{
    trackers: ShardedMap<A>,
    count: AtomicUsize,
    has_no_attribute_value: AtomicBool,
    no_attribute_tracker: TrackerEntry<A>,
    /// Pre-hashed overflow attributes — computed once at construction.
    overflow_attrs: HashedAttributes,
    config: A::InitConfig,
    cardinality_limit: usize,
}

impl<A> ValueMap<A>
where
    A: Aggregator,
{
    pub(crate) fn config(&self) -> &A::InitConfig {
        &self.config
    }

    fn new(config: A::InitConfig, cardinality_limit: usize) -> Self {
        let capacity_per_shard =
            (1 + min(DEFAULT_CARDINALITY_LIMIT, cardinality_limit)) / NUM_SHARDS + 1;
        ValueMap {
            trackers: ShardedMap::new(capacity_per_shard),
            has_no_attribute_value: AtomicBool::new(false),
            no_attribute_tracker: TrackerEntry::new(A::create(&config)),
            count: AtomicUsize::new(0),
            overflow_attrs: HashedAttributes::from_sorted(stream_overflow_attributes().clone()),
            config,
            cardinality_limit,
        }
    }

    fn measure(&self, value: A::PreComputedValue, attributes: &[KeyValue]) {
        if attributes.is_empty() {
            self.no_attribute_tracker.aggregator.update(value);
            self.no_attribute_tracker
                .has_been_updated
                .store(true, Ordering::Release);
            self.has_no_attribute_value.store(true, Ordering::Release);
            return;
        }

        // Hash once, sort+dedup once — this replaces the two-lookup pattern
        let hashed = HashedAttributes::from_raw(attributes);

        self.trackers.measure(
            &hashed,
            value,
            &self.config,
            &self.count,
            self.cardinality_limit,
            &self.overflow_attrs,
        );
    }

    /// Iterate through all attribute sets and populate `DataPoints` in readonly mode.
    /// This is used for synchronous instruments (Counter, Histogram, etc.) in Cumulative temporality mode,
    /// where attribute sets persist across collection cycles and [`ValueMap`] is not cleared.
    pub(crate) fn collect_readonly<Res, MapFn>(&self, dest: &mut Vec<Res>, mut map_fn: MapFn)
    where
        MapFn: FnMut(Vec<KeyValue>, &A) -> Res,
    {
        prepare_data(dest, self.count.load(Ordering::SeqCst));
        if self.has_no_attribute_value.load(Ordering::Acquire) {
            dest.push(map_fn(vec![], &self.no_attribute_tracker.aggregator));
        }

        self.trackers.for_each(|attrs, entry| {
            dest.push(map_fn(attrs.attrs.clone(), &entry.aggregator));
        });
    }

    /// Iterate through all attribute sets in-place, populate `DataPoints` and reset.
    /// Unlike `drain_and_reset`, this keeps entries in the map and only processes
    /// those that have been updated since the last collection (tracked via the
    /// `has_been_updated` flag in TrackerEntry). This avoids heap allocations and
    /// map rebuilding that occurs with drain-based collection.
    ///
    /// Entries that were not updated since the last collection are evicted from the
    /// map to prevent unbounded memory growth from dynamic attribute sets.
    ///
    /// Used for synchronous instruments (Counter, Gauge) in Delta temporality mode.
    pub(crate) fn collect_and_reset<Res, MapFn>(&self, dest: &mut Vec<Res>, mut map_fn: MapFn)
    where
        MapFn: FnMut(Vec<KeyValue>, &A) -> Res,
    {
        prepare_data(dest, self.count.load(Ordering::SeqCst));
        if self.has_no_attribute_value.load(Ordering::Acquire) {
            if self
                .no_attribute_tracker
                .has_been_updated
                .swap(false, Ordering::AcqRel)
            {
                dest.push(map_fn(vec![], &self.no_attribute_tracker.aggregator));
            }
        }

        // Collect updated entries and count stale ones
        let mut has_stale = false;
        self.trackers.for_each(|attrs, entry| {
            if entry.has_been_updated.swap(false, Ordering::Relaxed) {
                dest.push(map_fn(attrs.attrs.clone(), &entry.aggregator));
            } else if *attrs != self.overflow_attrs {
                has_stale = true;
            }
        });

        // Evict stale entries
        if has_stale {
            let mut evicted = 0usize;
            self.trackers.retain(|attrs, entry| {
                if entry.has_been_updated.load(Ordering::Relaxed)
                    || *attrs == self.overflow_attrs
                {
                    true
                } else {
                    evicted += 1;
                    false
                }
            });
            if evicted > 0 {
                self.count.fetch_sub(evicted, Ordering::SeqCst);
            }
        }
    }

    /// Iterate through all attribute sets, populate `DataPoints` and reset by draining the map.
    /// This is used for asynchronous instruments (Observable/PrecomputedSum) in both Delta and
    /// Cumulative temporality modes, where map clearing is needed for staleness detection.
    pub(crate) fn drain_and_reset<Res, MapFn>(&self, dest: &mut Vec<Res>, mut map_fn: MapFn)
    where
        MapFn: FnMut(Vec<KeyValue>, A) -> Res,
    {
        prepare_data(dest, self.count.load(Ordering::SeqCst));
        if self.has_no_attribute_value.swap(false, Ordering::AcqRel) {
            dest.push(map_fn(
                vec![],
                self.no_attribute_tracker
                    .aggregator
                    .clone_and_reset(&self.config),
            ));
        }

        let old_entries = self.trackers.drain_all();
        self.count.store(0, Ordering::SeqCst);

        for (attrs, entry) in old_entries {
            dest.push(map_fn(attrs.attrs, entry.aggregator.clone_and_reset(&self.config)));
        }
    }
}

/// Clear and allocate exactly required amount of space for all attribute-sets
fn prepare_data<T>(data: &mut Vec<T>, list_len: usize) {
    data.clear();
    let total_len = list_len + 2; // to account for no_attributes case + overflow state
    if total_len > data.capacity() {
        data.reserve_exact(total_len - data.capacity());
    }
}

/// Marks a type that can have a value added and retrieved atomically. Required since
/// different types have different backing atomic mechanisms
pub(crate) trait AtomicTracker<T>: Sync + Send + 'static {
    fn store(&self, _value: T);
    fn add(&self, _value: T);
    fn get_value(&self) -> T;
    fn get_and_reset_value(&self) -> T;
}

/// Marks a type that can have an atomic tracker generated for it
pub(crate) trait AtomicallyUpdate<T> {
    type AtomicTracker: AtomicTracker<T>;
    fn new_atomic_tracker(init: T) -> Self::AtomicTracker;
}

pub(crate) trait AggregatedMetricsAccess: Sized {
    /// This function is used in tests.
    #[allow(unused)]
    fn extract_metrics_data_ref(data: &AggregatedMetrics) -> Option<&MetricData<Self>>;
    fn extract_metrics_data_mut(data: &mut AggregatedMetrics) -> Option<&mut MetricData<Self>>;
    fn make_aggregated_metrics(data: MetricData<Self>) -> AggregatedMetrics;
}

pub(crate) trait Number:
    Add<Output = Self>
    + AddAssign
    + Sub<Output = Self>
    + PartialOrd
    + fmt::Debug
    + Clone
    + Copy
    + PartialEq
    + Default
    + Send
    + Sync
    + 'static
    + AtomicallyUpdate<Self>
    + AggregatedMetricsAccess
{
    fn min() -> Self;
    fn max() -> Self;

    fn into_float(self) -> f64;
}

impl Number for i64 {
    fn min() -> Self {
        i64::MIN
    }

    fn max() -> Self {
        i64::MAX
    }

    fn into_float(self) -> f64 {
        // May have precision loss at high values
        self as f64
    }
}
impl Number for u64 {
    fn min() -> Self {
        u64::MIN
    }

    fn max() -> Self {
        u64::MAX
    }

    fn into_float(self) -> f64 {
        // May have precision loss at high values
        self as f64
    }
}
impl Number for f64 {
    fn min() -> Self {
        f64::MIN
    }

    fn max() -> Self {
        f64::MAX
    }

    fn into_float(self) -> f64 {
        self
    }
}

impl AggregatedMetricsAccess for i64 {
    fn make_aggregated_metrics(data: MetricData<i64>) -> AggregatedMetrics {
        AggregatedMetrics::I64(data)
    }

    fn extract_metrics_data_ref(data: &AggregatedMetrics) -> Option<&MetricData<i64>> {
        if let AggregatedMetrics::I64(data) = data {
            Some(data)
        } else {
            None
        }
    }

    fn extract_metrics_data_mut(data: &mut AggregatedMetrics) -> Option<&mut MetricData<i64>> {
        if let AggregatedMetrics::I64(data) = data {
            Some(data)
        } else {
            None
        }
    }
}

impl AggregatedMetricsAccess for u64 {
    fn make_aggregated_metrics(data: MetricData<u64>) -> AggregatedMetrics {
        AggregatedMetrics::U64(data)
    }

    fn extract_metrics_data_ref(data: &AggregatedMetrics) -> Option<&MetricData<u64>> {
        if let AggregatedMetrics::U64(data) = data {
            Some(data)
        } else {
            None
        }
    }

    fn extract_metrics_data_mut(data: &mut AggregatedMetrics) -> Option<&mut MetricData<u64>> {
        if let AggregatedMetrics::U64(data) = data {
            Some(data)
        } else {
            None
        }
    }
}

impl AggregatedMetricsAccess for f64 {
    fn make_aggregated_metrics(data: MetricData<f64>) -> AggregatedMetrics {
        AggregatedMetrics::F64(data)
    }

    fn extract_metrics_data_ref(data: &AggregatedMetrics) -> Option<&MetricData<f64>> {
        if let AggregatedMetrics::F64(data) = data {
            Some(data)
        } else {
            None
        }
    }

    fn extract_metrics_data_mut(data: &mut AggregatedMetrics) -> Option<&mut MetricData<f64>> {
        if let AggregatedMetrics::F64(data) = data {
            Some(data)
        } else {
            None
        }
    }
}

impl AtomicTracker<u64> for AtomicU64 {
    fn store(&self, value: u64) {
        self.store(value, Ordering::Relaxed);
    }

    fn add(&self, value: u64) {
        self.fetch_add(value, Ordering::Relaxed);
    }

    fn get_value(&self) -> u64 {
        self.load(Ordering::Relaxed)
    }

    fn get_and_reset_value(&self) -> u64 {
        self.swap(0, Ordering::Relaxed)
    }
}

impl AtomicallyUpdate<u64> for u64 {
    type AtomicTracker = AtomicU64;

    fn new_atomic_tracker(init: u64) -> Self::AtomicTracker {
        AtomicU64::new(init)
    }
}

impl AtomicTracker<i64> for AtomicI64 {
    fn store(&self, value: i64) {
        self.store(value, Ordering::Relaxed);
    }

    fn add(&self, value: i64) {
        self.fetch_add(value, Ordering::Relaxed);
    }

    fn get_value(&self) -> i64 {
        self.load(Ordering::Relaxed)
    }

    fn get_and_reset_value(&self) -> i64 {
        self.swap(0, Ordering::Relaxed)
    }
}

impl AtomicallyUpdate<i64> for i64 {
    type AtomicTracker = AtomicI64;

    fn new_atomic_tracker(init: i64) -> Self::AtomicTracker {
        AtomicI64::new(init)
    }
}

pub(crate) struct F64AtomicTracker {
    inner: AtomicU64, // Floating points don't have true atomics, so we need to use the their binary representation to perform atomic operations
}

impl F64AtomicTracker {
    fn new(init: f64) -> Self {
        let value_as_u64 = init.to_bits();
        F64AtomicTracker {
            inner: AtomicU64::new(value_as_u64),
        }
    }
}

impl AtomicTracker<f64> for F64AtomicTracker {
    fn store(&self, value: f64) {
        let value_as_u64 = value.to_bits();
        self.inner.store(value_as_u64, Ordering::Relaxed);
    }

    fn add(&self, value: f64) {
        let mut current_value_as_u64 = self.inner.load(Ordering::Relaxed);

        loop {
            let current_value = f64::from_bits(current_value_as_u64);
            let new_value = current_value + value;
            let new_value_as_u64 = new_value.to_bits();
            match self.inner.compare_exchange(
                current_value_as_u64,
                new_value_as_u64,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                // Succeeded in updating the value
                Ok(_) => return,

                // Some other thread changed the value before this thread could update it.
                // Read the latest value again and try to swap it with the recomputed `new_value_as_u64`.
                Err(v) => current_value_as_u64 = v,
            }
        }
    }

    fn get_value(&self) -> f64 {
        let value_as_u64 = self.inner.load(Ordering::Relaxed);
        f64::from_bits(value_as_u64)
    }

    fn get_and_reset_value(&self) -> f64 {
        let zero_as_u64 = 0.0_f64.to_bits();
        let value = self.inner.swap(zero_as_u64, Ordering::Relaxed);
        f64::from_bits(value)
    }
}

impl AtomicallyUpdate<f64> for f64 {
    type AtomicTracker = F64AtomicTracker;

    fn new_atomic_tracker(init: f64) -> Self::AtomicTracker {
        F64AtomicTracker::new(init)
    }
}

#[cfg(test)]
mod tests {
    use crate::metrics::internal::last_value::Assign;
    use super::*;

    // Test helpers that return boxed trait objects to avoid method shadowing
    // from portable-atomic's inherent methods
    fn new_u64_tracker(init: u64) -> Box<dyn AtomicTracker<u64>> {
        Box::new(u64::new_atomic_tracker(init))
    }

    fn new_i64_tracker(init: i64) -> Box<dyn AtomicTracker<i64>> {
        Box::new(i64::new_atomic_tracker(init))
    }

    #[test]
    fn can_store_u64_atomic_value() {
        let atomic = new_u64_tracker(0);

        let value = atomic.get_value();
        assert_eq!(value, 0);

        atomic.store(25);
        let value = atomic.get_value();
        assert_eq!(value, 25);
    }

    #[test]
    fn can_add_and_get_u64_atomic_value() {
        let atomic = new_u64_tracker(0);
        atomic.add(15);
        atomic.add(10);

        let value = atomic.get_value();
        assert_eq!(value, 25);
    }

    #[test]
    fn can_reset_u64_atomic_value() {
        let atomic = new_u64_tracker(0);
        atomic.add(15);

        let value = atomic.get_and_reset_value();
        let value2 = atomic.get_value();

        assert_eq!(value, 15, "Incorrect first value");
        assert_eq!(value2, 0, "Incorrect second value");
    }

    #[test]
    fn can_store_i64_atomic_value() {
        let atomic = new_i64_tracker(0);

        let value = atomic.get_value();
        assert_eq!(value, 0);

        atomic.store(-25);
        let value = atomic.get_value();
        assert_eq!(value, -25);

        atomic.store(25);
        let value = atomic.get_value();
        assert_eq!(value, 25);
    }

    #[test]
    fn can_add_and_get_i64_atomic_value() {
        let atomic = new_i64_tracker(0);
        atomic.add(15);
        atomic.add(-10);

        let value = atomic.get_value();
        assert_eq!(value, 5);
    }

    #[test]
    fn can_reset_i64_atomic_value() {
        let atomic = new_i64_tracker(0);
        atomic.add(15);

        let value = atomic.get_and_reset_value();
        let value2 = atomic.get_value();

        assert_eq!(value, 15, "Incorrect first value");
        assert_eq!(value2, 0, "Incorrect second value");
    }

    #[test]
    fn can_store_f64_atomic_value() {
        let atomic = f64::new_atomic_tracker(0.0);
        let atomic_tracker = &atomic as &dyn AtomicTracker<f64>;

        let value = atomic.get_value();
        assert_eq!(value, 0.0);

        atomic_tracker.store(-15.5);
        let value = atomic.get_value();
        assert!(f64::abs(-15.5 - value) < 0.0001);

        atomic_tracker.store(25.7);
        let value = atomic.get_value();
        assert!(f64::abs(25.7 - value) < 0.0001);
    }

    #[test]
    fn can_add_and_get_f64_atomic_value() {
        let atomic = f64::new_atomic_tracker(0.0);
        atomic.add(15.3);
        atomic.add(10.4);

        let value = atomic.get_value();

        assert!(f64::abs(25.7 - value) < 0.0001);
    }

    #[test]
    fn can_reset_f64_atomic_value() {
        let atomic = f64::new_atomic_tracker(0.0);
        atomic.add(15.5);

        let value = atomic.get_and_reset_value();
        let value2 = atomic.get_value();

        assert!(f64::abs(15.5 - value) < 0.0001, "Incorrect first value");
        assert!(f64::abs(0.0 - value2) < 0.0001, "Incorrect second value");
    }

    #[test]
    fn large_cardinality_limit() {
        // This is a regression test for panics that used to occur for large cardinality limits

        // Should not panic
        let _value_map = ValueMap::<Assign<i64>>::new((), usize::MAX);
    }

    #[test]
    fn hashed_attributes_equal_regardless_of_input_order() {
        let a = HashedAttributes::from_raw(&[
            KeyValue::new("z", "1"),
            KeyValue::new("a", "2"),
        ]);
        let b = HashedAttributes::from_raw(&[
            KeyValue::new("a", "2"),
            KeyValue::new("z", "1"),
        ]);
        assert_eq!(a, b);
        assert_eq!(a.hash, b.hash);
    }

    #[test]
    fn hashed_attributes_dedup_keys() {
        let a = HashedAttributes::from_raw(&[
            KeyValue::new("k", "first"),
            KeyValue::new("k", "second"),
        ]);
        assert_eq!(a.attrs.len(), 1);
    }

    #[test]
    fn hashed_attributes_different_values_differ() {
        let a = HashedAttributes::from_raw(&[KeyValue::new("k", "v1")]);
        let b = HashedAttributes::from_raw(&[KeyValue::new("k", "v2")]);
        assert_ne!(a, b);
    }

    #[test]
    fn passthrough_hasher_returns_precomputed_hash() {
        use std::hash::Hash;
        let ha = HashedAttributes::from_raw(&[KeyValue::new("test", "val")]);
        let mut hasher = PassthroughHasher(0);
        ha.hash(&mut hasher);
        assert_eq!(hasher.finish(), ha.hash);
    }

    #[test]
    fn sharded_map_measure_and_iterate() {
        let map = ShardedMap::<Assign<i64>>::new(16);
        let count = AtomicUsize::new(0);
        let attrs = HashedAttributes::from_raw(&[KeyValue::new("k", "v")]);
        let overflow = HashedAttributes::from_sorted(stream_overflow_attributes().clone());

        // First measure creates entry
        map.measure(&attrs, 10, &(), &count, 2000, &overflow);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Second measure updates existing (Assign stores last value)
        map.measure(&attrs, 42, &(), &count, 2000, &overflow);
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Verify last value via iteration
        let mut val = 0i64;
        map.for_each(|_, entry| {
            val = entry.aggregator.value.get_value();
        });
        assert_eq!(val, 42);
    }

    #[test]
    fn sharded_map_cardinality_limit() {
        let map = ShardedMap::<Assign<i64>>::new(4);
        let count = AtomicUsize::new(0);
        let overflow = HashedAttributes::from_sorted(stream_overflow_attributes().clone());

        // Insert up to limit
        for i in 0..3 {
            let attrs = HashedAttributes::from_raw(&[KeyValue::new("k", i.to_string())]);
            map.measure(&attrs, 1, &(), &count, 3, &overflow);
        }
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // Next insert should go to overflow bucket
        let attrs = HashedAttributes::from_raw(&[KeyValue::new("k", "new")]);
        map.measure(&attrs, 99, &(), &count, 3, &overflow);
        // Count should still be 3 (overflow doesn't increment count)
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // Verify overflow bucket got the value
        let mut overflow_val = 0i64;
        map.for_each(|attrs, entry| {
            if *attrs == overflow {
                overflow_val = entry.aggregator.value.get_value();
            }
        });
        assert_eq!(overflow_val, 99);
    }

    #[test]
    fn sharded_map_retain_evicts_entries() {
        let map = ShardedMap::<Assign<i64>>::new(4);
        let count = AtomicUsize::new(0);
        let overflow = HashedAttributes::from_sorted(stream_overflow_attributes().clone());

        let a1 = HashedAttributes::from_raw(&[KeyValue::new("k", "keep")]);
        let a2 = HashedAttributes::from_raw(&[KeyValue::new("k", "evict")]);
        map.measure(&a1, 1, &(), &count, 2000, &overflow);
        map.measure(&a2, 1, &(), &count, 2000, &overflow);

        // Mark a2 as not updated (simulating stale)
        map.for_each(|attrs, entry| {
            if attrs.attrs[0].key.as_str() == "k"
                && format!("{}", attrs.attrs[0].value) == "evict"
            {
                entry.has_been_updated.store(false, Ordering::Relaxed);
            }
        });

        map.retain(|_, entry| entry.has_been_updated.load(Ordering::Relaxed));

        let mut remaining = 0;
        map.for_each(|_, _| remaining += 1);
        assert_eq!(remaining, 1);
    }
}
