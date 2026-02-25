# EXP-004: Bound Instruments Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Benchmark PoC that caches resolved tracker references in bound instrument handles, skipping per-call sort+hash+lock+lookup on the measure() hot path.

**Architecture:** Add `evicted` flag to `TrackerEntry`, wrap map values in `Arc`, add `bind()` method at each layer (ValueMap → Measure trait → SyncInstrument trait → Counter/Histogram). Bound handles check poison flag and fall back to unbound path on eviction.

**Tech Stack:** Rust, `std::sync::Arc`, `AtomicBool`, criterion benchmarks.

---

### Task 1: Add `evicted` flag to TrackerEntry and wrap in Arc

**Files:**
- Modify: `opentelemetry-sdk/src/metrics/internal/mod.rs:144-156` (TrackerEntry)
- Modify: `opentelemetry-sdk/src/metrics/internal/mod.rs:163` (ShardHashMap type alias)
- Modify: `opentelemetry-sdk/src/metrics/internal/mod.rs:168-274` (ShardedMap methods)
- Modify: `opentelemetry-sdk/src/metrics/internal/mod.rs:280-434` (ValueMap methods)

This is the foundational change. Every method that touches `TrackerEntry` must account for the `Arc` wrapper and the new `evicted` field.

**Step 1: Add `evicted` to TrackerEntry**

In `mod.rs:144-156`, add the new field:

```rust
struct TrackerEntry<A> {
    aggregator: A,
    has_been_updated: AtomicBool,
    evicted: AtomicBool,
}

impl<A: Aggregator> TrackerEntry<A> {
    fn new(aggregator: A) -> Self {
        TrackerEntry {
            aggregator,
            has_been_updated: AtomicBool::new(true),
            evicted: AtomicBool::new(false),
        }
    }
}
```

**Step 2: Change ShardHashMap to use Arc**

At `mod.rs:163`:

```rust
type ShardHashMap<A> = HashMap<HashedAttributes, Arc<TrackerEntry<A>>, PassthroughBuildHasher>;
```

**Step 3: Update ShardedMap::measure() for Arc**

At `mod.rs:192-237`. Key changes:
- `shard.get()` returns `&Arc<TrackerEntry<A>>` — no change needed for read access (Deref)
- `shard.insert()` now wraps in `Arc::new()`
- Overflow path similarly wraps in `Arc::new()`

```rust
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

    if count.load(Ordering::SeqCst) < cardinality_limit {
        let new_entry = Arc::new(TrackerEntry::new(A::create(config)));
        new_entry.aggregator.update(value);
        shard.insert(hashed_attrs.clone(), new_entry);
        count.fetch_add(1, Ordering::SeqCst);
        return;
    }

    drop(shard);

    let overflow_idx = Self::shard_index(overflow_attrs.hash);
    let mut overflow_shard = self.shards[overflow_idx]
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = overflow_shard.get(overflow_attrs) {
        entry.aggregator.update(value);
        entry.has_been_updated.store(true, Ordering::Relaxed);
    } else {
        let entry = Arc::new(TrackerEntry::new(A::create(config)));
        entry.aggregator.update(value);
        overflow_shard.insert(overflow_attrs.clone(), entry);
    }
}
```

**Step 4: Update ShardedMap::for_each, retain, drain_all signatures**

The callback signatures change because values are now `Arc<TrackerEntry<A>>`:

```rust
fn for_each<F>(&self, mut f: F)
where
    F: FnMut(&HashedAttributes, &Arc<TrackerEntry<A>>),
{ /* body unchanged — iterates &Arc<TrackerEntry<A>> */ }

fn retain<F>(&self, mut f: F)
where
    F: FnMut(&HashedAttributes, &Arc<TrackerEntry<A>>) -> bool,
{ /* body unchanged */ }

fn drain_all(&self) -> Vec<(HashedAttributes, Arc<TrackerEntry<A>>)> {
    /* body unchanged — drains Arc values */
}
```

**Step 5: Add `bind()` method to ShardedMap**

New method on ShardedMap:

```rust
/// Resolve or create a tracker entry for the given attributes.
/// Returns an Arc clone that the caller can cache for direct access.
fn bind(
    &self,
    hashed_attrs: &HashedAttributes,
    config: &A::InitConfig,
    count: &AtomicUsize,
    cardinality_limit: usize,
    overflow_attrs: &HashedAttributes,
) -> Arc<TrackerEntry<A>> {
    let idx = Self::shard_index(hashed_attrs.hash);
    let mut shard = self.shards[idx].lock().unwrap_or_else(|e| e.into_inner());

    if let Some(entry) = shard.get(hashed_attrs) {
        return Arc::clone(entry);
    }

    if count.load(Ordering::SeqCst) < cardinality_limit {
        let new_entry = Arc::new(TrackerEntry::new(A::create(config)));
        let cloned = Arc::clone(&new_entry);
        shard.insert(hashed_attrs.clone(), new_entry);
        count.fetch_add(1, Ordering::SeqCst);
        return cloned;
    }

    // Over cardinality limit — bind to overflow bucket
    drop(shard);
    let overflow_idx = Self::shard_index(overflow_attrs.hash);
    let mut overflow_shard = self.shards[overflow_idx]
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = overflow_shard.get(overflow_attrs) {
        Arc::clone(entry)
    } else {
        let entry = Arc::new(TrackerEntry::new(A::create(config)));
        let cloned = Arc::clone(&entry);
        overflow_shard.insert(overflow_attrs.clone(), entry);
        cloned
    }
}
```

**Step 6: Add `bind()` method to ValueMap**

```rust
impl<A: Aggregator> ValueMap<A> {
    pub(crate) fn bind(&self, attributes: &[KeyValue]) -> Arc<TrackerEntry<A>> {
        if attributes.is_empty() {
            // For no-attribute case, we cannot return the no_attribute_tracker
            // as it is not Arc-wrapped. Fall back to a lookup-based approach.
            // (This path is rare for bound instruments — users bind specific attr sets.)
            panic!("bind() with empty attributes is not supported; use add() directly");
        }

        let hashed = HashedAttributes::from_raw(attributes);
        self.trackers.bind(
            &hashed,
            &self.config,
            &self.count,
            self.cardinality_limit,
            &self.overflow_attrs,
        )
    }
}
```

**Step 7: Update `collect_and_reset` to set poison flag**

In `mod.rs:366-408`, modify the retain closure:

```rust
self.trackers.retain(|attrs, entry| {
    if entry.has_been_updated.load(Ordering::Relaxed)
        || *attrs == self.overflow_attrs
    {
        true
    } else {
        entry.evicted.store(true, Ordering::Release); // Poison before removal
        evicted += 1;
        false
    }
});
```

**Step 8: Update `ValueMap::new` to wrap `no_attribute_tracker`**

The `no_attribute_tracker` stays as a direct `TrackerEntry<A>` (not Arc-wrapped) since it is never shared with bound handles. No change needed.

**Step 9: Update `drain_and_reset` for Arc**

In `mod.rs:413-433`, the drain callback receives `Arc<TrackerEntry<A>>`. Update the map_fn call:

```rust
for (attrs, entry) in old_entries {
    // Set evicted on drained entries so any bound handles detect staleness
    entry.evicted.store(true, Ordering::Release);
    dest.push(map_fn(attrs.attrs, entry.aggregator.clone_and_reset(&self.config)));
}
```

Note: `entry.aggregator` works through `Arc::Deref`. The `clone_and_reset` call takes `&self` so this works.

**Step 10: Run existing tests to verify no regressions**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader -- metrics::internal`
Expected: All existing tests pass (the Arc wrapper is transparent for reads).

**Step 11: Add tests for bind() and poisoning**

Add to the test module in `mod.rs`:

```rust
#[test]
fn value_map_bind_returns_tracker() {
    let vm = ValueMap::<Assign<i64>>::new((), 2000);
    let attrs = [KeyValue::new("k", "v")];
    let entry = vm.bind(&attrs);
    entry.aggregator.update(42);
    assert_eq!(entry.aggregator.value.get_value(), 42);
}

#[test]
fn value_map_bind_same_attrs_returns_same_entry() {
    let vm = ValueMap::<Assign<i64>>::new((), 2000);
    let attrs = [KeyValue::new("k", "v")];
    let e1 = vm.bind(&attrs);
    let e2 = vm.bind(&attrs);
    e1.aggregator.update(10);
    assert_eq!(e2.aggregator.value.get_value(), 10); // Same underlying tracker
}

#[test]
fn value_map_bind_unsorted_attrs_canonicalized() {
    let vm = ValueMap::<Assign<i64>>::new((), 2000);
    let e1 = vm.bind(&[KeyValue::new("z", "1"), KeyValue::new("a", "2")]);
    let e2 = vm.bind(&[KeyValue::new("a", "2"), KeyValue::new("z", "1")]);
    e1.aggregator.update(99);
    assert_eq!(e2.aggregator.value.get_value(), 99);
}

#[test]
fn poisoning_sets_evicted_on_stale_entries() {
    let vm = ValueMap::<Assign<i64>>::new((), 2000);
    let attrs = [KeyValue::new("k", "v")];
    let entry = vm.bind(&attrs);
    assert!(!entry.evicted.load(Ordering::Acquire));

    // First collect_and_reset: marks entry as not-updated
    let mut dest = Vec::new();
    vm.collect_and_reset(&mut dest, |attrs, aggr| (attrs, aggr.value.get_value()));
    // Entry was updated at bind (TrackerEntry::new sets has_been_updated=true),
    // so first collect sees it as updated and collects it.

    // Second collect_and_reset: entry is stale (no writes since last collect)
    let mut dest2 = Vec::new();
    vm.collect_and_reset(&mut dest2, |attrs, aggr| (attrs, aggr.value.get_value()));
    // Now entry should be evicted and poisoned
    assert!(entry.evicted.load(Ordering::Acquire));
}
```

**Step 12: Run tests**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader -- metrics::internal`
Expected: All tests pass including new ones.

**Step 13: Commit**

```bash
git add opentelemetry-sdk/src/metrics/internal/mod.rs
git commit -m "exp-004: wrap TrackerEntry in Arc, add evicted flag and bind()"
```

---

### Task 2: Add BoundMeasure trait and implement for Sum (Counter)

**Files:**
- Modify: `opentelemetry-sdk/src/metrics/internal/aggregate.rs:18-21` (Measure trait)
- Modify: `opentelemetry-sdk/src/metrics/internal/sum.rs:147-156` (Sum Measure impl)
- Modify: `opentelemetry-sdk/src/metrics/internal/mod.rs:19` (re-export)

**Step 1: Define BoundMeasure trait and extend Measure trait**

In `aggregate.rs`, after the existing `Measure` trait (line 21):

```rust
/// A pre-bound measurement handle that skips attribute lookup.
/// Created by Measure::bind(), holds a direct reference to the tracker.
pub(crate) trait BoundMeasure<T>: Send + Sync + 'static {
    fn call(&self, measurement: T);
}

/// Receives measurements to be aggregated.
pub(crate) trait Measure<T>: Send + Sync + 'static {
    fn call(&self, measurement: T, attrs: &[KeyValue]);
    fn bind(&self, attrs: &[KeyValue]) -> Box<dyn BoundMeasure<T>>;
}
```

Update the re-export in `mod.rs:19`:

```rust
pub(crate) use aggregate::{AggregateBuilder, AggregateFns, BoundMeasure, ComputeAggregation, Measure};
```

**Step 2: Implement BoundMeasure for Sum (Counter)**

In `sum.rs`, after the existing `Measure<T> for Sum<T>` impl (after line 156):

```rust
use std::sync::Arc;
use super::TrackerEntry;

struct BoundSumHandle<T: Number> {
    entry: Arc<TrackerEntry<Increment<T>>>,
    fallback: Arc<dyn Measure<T>>,
    attrs: Vec<KeyValue>,
}

impl<T: Number> BoundMeasure<T> for BoundSumHandle<T> {
    fn call(&self, value: T) {
        if self.entry.evicted.load(std::sync::atomic::Ordering::Acquire) {
            // Poisoned — fall back to unbound path
            self.fallback.call(value, &self.attrs);
            return;
        }
        self.entry.aggregator.update(value);
        self.entry.has_been_updated.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
```

Update the `Measure<T> for Sum<T>` impl to add `bind()`:

```rust
impl<T> Measure<T> for Sum<T>
where
    T: Number,
{
    fn call(&self, measurement: T, attrs: &[KeyValue]) {
        self.filter.apply(attrs, |filtered| {
            self.value_map.measure(measurement, filtered);
        })
    }

    fn bind(&self, attrs: &[KeyValue]) -> Box<dyn BoundMeasure<T>> {
        // Apply filter at bind time (if any)
        let filtered: Vec<KeyValue> = if let Some(ref filter) = self.filter.filter {
            attrs.iter().filter(|kv| filter(kv)).cloned().collect()
        } else {
            attrs.to_vec()
        };
        let entry = self.value_map.bind(&filtered);
        Box::new(BoundSumHandle {
            entry,
            fallback: Arc::new(/* need self reference — see note */),
            attrs: filtered,
        })
    }
}
```

**Important design note:** The `BoundSumHandle` needs an `Arc<dyn Measure<T>>` for fallback. But `Sum<T>` implements `Measure<T>` and is already stored in `Arc<dyn Measure<T>>` inside `ResolvedMeasures`. We need to pass `self` as the fallback. Since `Measure::bind` takes `&self`, we cannot get an `Arc<Self>` from `&self`.

**Solution:** Change `bind` signature to accept the `Arc<dyn Measure<T>>` as a parameter:

```rust
pub(crate) trait Measure<T>: Send + Sync + 'static {
    fn call(&self, measurement: T, attrs: &[KeyValue]);
    fn bind(&self, attrs: &[KeyValue], self_arc: Arc<dyn Measure<T>>) -> Box<dyn BoundMeasure<T>>;
}
```

This way the caller (`ResolvedMeasures`) passes the `Arc` it already holds.

Alternatively, for simplicity in the PoC, the fallback can store a clone of the `ValueMap` reference and call `measure()` directly. But since `ValueMap` is owned by `Sum<T>`, this is tricky.

**Simplest PoC approach:** Store the raw attributes and re-do the full path manually in the fallback:

```rust
struct BoundSumHandle<T: Number> {
    entry: Arc<TrackerEntry<Increment<T>>>,
    // For fallback after poisoning, we store what we need to call ValueMap::measure directly
    fallback_value_map: *const ValueMap<Increment<T>>,  // Raw pointer — PoC only
    attrs: Vec<KeyValue>,
}
```

**Actually, even simpler:** For the PoC, since we're benchmarking steady-state (no eviction), the fallback path is never exercised during benchmarks. We can use a simple panic in the fallback:

```rust
impl<T: Number> BoundMeasure<T> for BoundSumHandle<T> {
    fn call(&self, value: T) {
        if self.entry.evicted.load(Ordering::Acquire) {
            panic!("BoundSumHandle: entry was evicted; re-bind required (PoC limitation)");
        }
        self.entry.aggregator.update(value);
        self.entry.has_been_updated.store(true, Ordering::Relaxed);
    }
}
```

Wait — the user explicitly asked for poisoning with fallback, not panic. Let's use the `Arc<dyn Measure<T>>` parameter approach.

**Final Measure trait:**

```rust
pub(crate) trait Measure<T>: Send + Sync + 'static {
    fn call(&self, measurement: T, attrs: &[KeyValue]);
    fn bind(&self, attrs: &[KeyValue], self_arc: Arc<dyn Measure<T>>) -> Box<dyn BoundMeasure<T>>;
}
```

**Final BoundSumHandle:**

```rust
struct BoundSumHandle<T: Number> {
    entry: Arc<TrackerEntry<Increment<T>>>,
    fallback: Arc<dyn Measure<T>>,
    attrs: Vec<KeyValue>,
}

impl<T: Number> BoundMeasure<T> for BoundSumHandle<T> {
    fn call(&self, value: T) {
        if self.entry.evicted.load(Ordering::Acquire) {
            self.fallback.call(value, &self.attrs);
            return;
        }
        self.entry.aggregator.update(value);
        self.entry.has_been_updated.store(true, Ordering::Relaxed);
    }
}

impl<T: Number> Measure<T> for Sum<T> {
    fn call(&self, measurement: T, attrs: &[KeyValue]) {
        self.filter.apply(attrs, |filtered| {
            self.value_map.measure(measurement, filtered);
        })
    }

    fn bind(&self, attrs: &[KeyValue], self_arc: Arc<dyn Measure<T>>) -> Box<dyn BoundMeasure<T>> {
        let filtered: Vec<KeyValue> = match &self.filter.filter {
            Some(filter) => attrs.iter().filter(|kv| filter(kv)).cloned().collect(),
            None => attrs.to_vec(),
        };
        let entry = self.value_map.bind(&filtered);
        Box::new(BoundSumHandle {
            entry,
            fallback: self_arc,
            attrs: filtered,
        })
    }
}
```

**Step 3: Expose `filter` field for bind() access**

The `AttributeSetFilter::filter` field is currently private. Add a pub(crate) getter or make the field pub(crate):

In `aggregate.rs:97-99`:

```rust
pub(crate) struct AttributeSetFilter {
    pub(crate) filter: Option<Filter>,
}
```

(It's already `pub(crate)` struct, just need to check if `filter` field is accessible. If not, add a method.)

**Step 4: Run tests**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader -- metrics::internal`
Expected: All tests pass. (Sum's bind() is not called by existing tests yet.)

**Step 5: Commit**

```bash
git add opentelemetry-sdk/src/metrics/internal/aggregate.rs opentelemetry-sdk/src/metrics/internal/sum.rs opentelemetry-sdk/src/metrics/internal/mod.rs
git commit -m "exp-004: add BoundMeasure trait, implement for Sum (counter)"
```

---

### Task 3: Implement BoundMeasure for Histogram

**Files:**
- Modify: `opentelemetry-sdk/src/metrics/internal/histogram.rs:222-239` (Histogram Measure impl)

**Step 1: Add BoundHistogramHandle and implement bind()**

In `histogram.rs`, after the `Measure<T> for Histogram<T>` impl:

```rust
use std::sync::Arc;
use super::{TrackerEntry, BoundMeasure};

struct BoundHistogramHandle<T: Number> {
    entry: Arc<TrackerEntry<Mutex<Buckets<T>>>>,
    bounds: Vec<f64>,
    fallback: Arc<dyn Measure<T>>,
    attrs: Vec<KeyValue>,
}

impl<T: Number> BoundMeasure<T> for BoundHistogramHandle<T> {
    fn call(&self, value: T) {
        if self.entry.evicted.load(std::sync::atomic::Ordering::Acquire) {
            self.fallback.call(value, &self.attrs);
            return;
        }
        let f = value.into_float();
        let index = self.bounds.partition_point(|&x| x < f);
        self.entry.aggregator.update((value, index));
        self.entry.has_been_updated.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl<T: Number> Measure<T> for Histogram<T> {
    fn call(&self, measurement: T, attrs: &[KeyValue]) {
        let f = measurement.into_float();
        let index = self.bounds.partition_point(|&x| x < f);
        self.filter.apply(attrs, |filtered| {
            self.value_map.measure((measurement, index), filtered);
        })
    }

    fn bind(&self, attrs: &[KeyValue], self_arc: Arc<dyn Measure<T>>) -> Box<dyn BoundMeasure<T>> {
        let filtered: Vec<KeyValue> = match &self.filter.filter {
            Some(filter) => attrs.iter().filter(|kv| filter(kv)).cloned().collect(),
            None => attrs.to_vec(),
        };
        let entry = self.value_map.bind(&filtered);
        Box::new(BoundHistogramHandle {
            entry,
            bounds: self.bounds.clone(),
            fallback: self_arc,
            attrs: filtered,
        })
    }
}
```

**Step 2: Implement bind() for remaining aggregator types (stub)**

For `LastValue`, `PrecomputedSum`, and `ExponentialHistogram` — add a simple stub that panics (PoC only targets Counter and Histogram):

In each file (`last_value.rs`, `precomputed_sum.rs`, `exponential_histogram.rs`), update their `Measure` impl:

```rust
fn bind(&self, _attrs: &[KeyValue], _self_arc: Arc<dyn Measure<T>>) -> Box<dyn BoundMeasure<T>> {
    unimplemented!("bind() not supported for this aggregator in PoC")
}
```

**Step 3: Run tests**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader -- metrics::internal`
Expected: All tests pass.

**Step 4: Commit**

```bash
git add opentelemetry-sdk/src/metrics/internal/
git commit -m "exp-004: implement BoundMeasure for Histogram, stub others"
```

---

### Task 4: Wire bind() through ResolvedMeasures and SyncInstrument

**Files:**
- Modify: `opentelemetry/src/metrics/instruments/mod.rs:28-31` (SyncInstrument trait)
- Modify: `opentelemetry-sdk/src/metrics/instrument.rs:381-391` (ResolvedMeasures)
- Modify: `opentelemetry-sdk/src/metrics/noop.rs` (NoopSyncInstrument)

**Step 1: Extend SyncInstrument trait with bind()**

In `opentelemetry/src/metrics/instruments/mod.rs:28-31`:

```rust
pub trait SyncInstrument<T>: Send + Sync {
    fn measure(&self, measurement: T, attributes: &[KeyValue]);
    fn bind(&self, attributes: &[KeyValue]) -> Vec<Box<dyn BoundMeasure<T>>>;
}
```

Wait — `BoundMeasure` is defined in `opentelemetry-sdk`, but `SyncInstrument` is in `opentelemetry` (the API crate). The API crate cannot depend on the SDK crate.

**Alternative approach:** Define a simpler trait in the API crate, or use a different wiring.

**Simplest PoC approach:** Don't modify `SyncInstrument` at all. Instead, add `bind()` directly on `Counter` and `Histogram` by downcasting to `ResolvedMeasures`.

Actually, for the PoC, the cleanest approach is:

1. Define `BoundSyncInstrument<T>` in the API crate with just `fn measure(&self, value: T)`
2. Add `fn bind(&self, attrs: &[KeyValue]) -> Box<dyn BoundSyncInstrument<T>>` to `SyncInstrument`
3. The SDK's `ResolvedMeasures` implements it

```rust
// In opentelemetry/src/metrics/instruments/mod.rs:

/// A pre-bound instrument handle that records measurements without attribute lookup.
pub trait BoundSyncInstrument<T>: Send + Sync {
    fn measure(&self, measurement: T);
}

pub trait SyncInstrument<T>: Send + Sync {
    fn measure(&self, measurement: T, attributes: &[KeyValue]);
    fn bind(&self, attributes: &[KeyValue]) -> Box<dyn BoundSyncInstrument<T>>;
}
```

**Step 2: Implement BoundSyncInstrument wrapper in SDK**

In `opentelemetry-sdk/src/metrics/instrument.rs`, add:

```rust
use opentelemetry::metrics::BoundSyncInstrument;
use crate::metrics::internal::BoundMeasure;

/// Wraps multiple BoundMeasure handles (one per pipeline) into a single BoundSyncInstrument.
struct ResolvedBoundMeasures<T> {
    handles: Vec<Box<dyn BoundMeasure<T>>>,
}

impl<T: Copy + 'static> BoundSyncInstrument<T> for ResolvedBoundMeasures<T> {
    fn measure(&self, val: T) {
        for handle in &self.handles {
            handle.call(val)
        }
    }
}
```

**Step 3: Implement bind() on ResolvedMeasures**

In `instrument.rs:385-391`:

```rust
impl<T: Copy + 'static> SyncInstrument<T> for ResolvedMeasures<T> {
    fn measure(&self, val: T, attrs: &[KeyValue]) {
        for measure in &self.measures {
            measure.call(val, attrs)
        }
    }

    fn bind(&self, attrs: &[KeyValue]) -> Box<dyn BoundSyncInstrument<T>> {
        let handles: Vec<Box<dyn BoundMeasure<T>>> = self.measures
            .iter()
            .map(|m| m.bind(attrs, Arc::clone(m)))
            .collect();
        Box::new(ResolvedBoundMeasures { handles })
    }
}
```

**Step 4: Update NoopSyncInstrument**

In `noop.rs`:

```rust
impl<T> SyncInstrument<T> for NoopSyncInstrument {
    fn measure(&self, _measurement: T, _attributes: &[KeyValue]) {}

    fn bind(&self, _attributes: &[KeyValue]) -> Box<dyn BoundSyncInstrument<T>> {
        Box::new(NoopBoundSyncInstrument)
    }
}

struct NoopBoundSyncInstrument;

impl<T> BoundSyncInstrument<T> for NoopBoundSyncInstrument {
    fn measure(&self, _measurement: T) {}
}
```

**Step 5: Run tests**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader`
Expected: All tests pass.

**Step 6: Commit**

```bash
git add opentelemetry/src/metrics/instruments/mod.rs opentelemetry-sdk/src/metrics/instrument.rs opentelemetry-sdk/src/metrics/noop.rs
git commit -m "exp-004: wire bind() through SyncInstrument and ResolvedMeasures"
```

---

### Task 5: Add BoundCounter and BoundHistogram public types

**Files:**
- Modify: `opentelemetry/src/metrics/instruments/counter.rs` (add BoundCounter, Counter::bind)
- Modify: `opentelemetry/src/metrics/instruments/histogram.rs` (add BoundHistogram, Histogram::bind)
- Modify: `opentelemetry/src/metrics/mod.rs` (re-export new types)

**Step 1: Add BoundCounter**

In `counter.rs`:

```rust
use super::BoundSyncInstrument;

/// A pre-bound counter handle that records measurements without per-call attribute lookup.
///
/// Created by [`Counter::bind`]. Holds a cached reference to the underlying aggregator.
pub struct BoundCounter<T>(Box<dyn BoundSyncInstrument<T> + Send + Sync>);

impl<T> BoundCounter<T> {
    pub fn add(&self, value: T) {
        self.0.measure(value)
    }
}

impl<T> Counter<T> {
    pub fn bind(&self, attributes: &[KeyValue]) -> BoundCounter<T> {
        BoundCounter(self.0.bind(attributes))
    }
}
```

**Step 2: Add BoundHistogram**

In `histogram.rs`:

```rust
use super::BoundSyncInstrument;

/// A pre-bound histogram handle that records measurements without per-call attribute lookup.
///
/// Created by [`Histogram::bind`]. Holds a cached reference to the underlying aggregator.
pub struct BoundHistogram<T>(Box<dyn BoundSyncInstrument<T> + Send + Sync>);

impl<T> BoundHistogram<T> {
    pub fn record(&self, value: T) {
        self.0.measure(value)
    }
}

impl<T> Histogram<T> {
    pub fn bind(&self, attributes: &[KeyValue]) -> BoundHistogram<T> {
        BoundHistogram(self.0.bind(attributes))
    }
}
```

**Step 3: Re-export new types**

In `opentelemetry/src/metrics/mod.rs`, add `BoundCounter` and `BoundHistogram` to the public exports.

**Step 4: Run full test suite**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry --features metrics && RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add opentelemetry/src/metrics/
git commit -m "exp-004: add BoundCounter and BoundHistogram public API"
```

---

### Task 6: Add benchmarks

**Files:**
- Modify: `opentelemetry-sdk/benches/metrics_delta_collect.rs`

**Step 1: Add bound counter benchmark**

Add a new benchmark group after the existing groups:

```rust
// ============================================================================
// BENCHMARK GROUP: Bound instruments (skip attribute lookup)
// ============================================================================
fn bench_bound_instruments(c: &mut Criterion) {
    let mut group = c.benchmark_group("BoundInstruments");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    // --- Bound Counter vs Unbound Counter (Delta) ---
    {
        let (_, counter) = setup_counter(Temporality::Delta);
        hydrate_counter(&counter);

        // Unbound baseline
        group.bench_function("Counter_Unbound_Delta", |b| {
            b.iter_batched(
                random_attrs_3,
                |attrs| counter.add(1, &attrs),
                BatchSize::SmallInput,
            );
        });

        // Bound: pre-bind a single attribute set
        let bound = counter.bind(&[
            KeyValue::new("attr1", "value1"),
            KeyValue::new("attr2", "value2"),
            KeyValue::new("attr3", "value3"),
        ]);
        group.bench_function("Counter_Bound_Delta", |b| {
            b.iter(|| bound.add(1));
        });
    }

    // --- Bound Histogram vs Unbound Histogram (Delta) ---
    {
        let (_, histogram) = setup_histogram(Temporality::Delta);
        hydrate_histogram(&histogram);

        group.bench_function("Histogram_Unbound_Delta", |b| {
            b.iter_batched(
                random_attrs_3,
                |attrs| histogram.record(42, &attrs),
                BatchSize::SmallInput,
            );
        });

        let bound_hist = histogram.bind(&[
            KeyValue::new("attr1", "value1"),
            KeyValue::new("attr2", "value2"),
            KeyValue::new("attr3", "value3"),
        ]);
        group.bench_function("Histogram_Bound_Delta", |b| {
            b.iter(|| bound_hist.record(42));
        });
    }

    // --- Multi-threaded bound counter ---
    for num_threads in [2, 4, 8] {
        let (_, counter) = setup_counter(Temporality::Delta);
        hydrate_counter(&counter);

        // Each thread gets its own bound handle to the same attribute set
        group.bench_function(
            BenchmarkId::new("Counter_Bound_Multithread", num_threads),
            |b| {
                b.iter_custom(|iters| {
                    let barrier = Arc::new(Barrier::new(num_threads));
                    let handles: Vec<_> = (0..num_threads)
                        .map(|_| {
                            let bound = counter.bind(&[
                                KeyValue::new("attr1", "value1"),
                                KeyValue::new("attr2", "value2"),
                                KeyValue::new("attr3", "value3"),
                            ]);
                            let barrier = barrier.clone();
                            std::thread::spawn(move || {
                                barrier.wait();
                                let start = std::time::Instant::now();
                                for _ in 0..iters {
                                    bound.add(1);
                                }
                                start.elapsed()
                            })
                        })
                        .collect();

                    let total: Duration =
                        handles.into_iter().map(|h| h.join().unwrap()).sum();
                    total / num_threads as u32
                });
            },
        );
    }

    group.finish();
}
```

**Step 2: Register the new benchmark group**

Update the `criterion_group!` and `criterion_main!` macros at the bottom of the file to include `bench_bound_instruments`.

**Step 3: Run benchmarks locally to verify they compile and execute**

Run: `RUSTFLAGS="--cap-lints warn" cargo bench -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader --bench metrics_delta_collect -- BoundInstruments --sample-size 10`
Expected: Benchmarks compile and run. Exact numbers will vary by machine.

**Step 4: Commit**

```bash
git add opentelemetry-sdk/benches/metrics_delta_collect.rs
git commit -m "exp-004: add bound instrument benchmarks"
```

---

### Task 7: Compile fixes and integration testing

This task handles any compilation errors from the previous tasks. The changes span two crates (`opentelemetry` API and `opentelemetry-sdk`) and touch trait definitions that many types implement.

**Step 1: Full build**

Run: `RUSTFLAGS="--cap-lints warn" cargo build -p opentelemetry --features metrics && RUSTFLAGS="--cap-lints warn" cargo build -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader`

Fix any compilation errors. Common issues:
- Missing `use` imports for `Arc`, `BoundMeasure`, `BoundSyncInstrument`
- Trait method signatures not matching between trait definition and impls
- `TrackerEntry` fields not being `pub(crate)` (needed for bind handle access)
- `AttributeSetFilter::filter` field visibility

**Step 2: Full test suite**

Run: `RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry --features metrics && RUSTFLAGS="--cap-lints warn" cargo test -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader`

**Step 3: Benchmark smoke test**

Run: `RUSTFLAGS="--cap-lints warn" cargo bench -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader --bench metrics_delta_collect -- BoundInstruments --sample-size 10`

**Step 4: Commit any fixes**

```bash
git add -A
git commit -m "exp-004: compilation fixes and integration test pass"
```

---

### Task 8: Run full benchmarks and record results

**Step 1: Run the complete benchmark suite**

Run: `RUSTFLAGS="--cap-lints warn" cargo bench -p opentelemetry-sdk --features metrics,experimental_metrics_custom_reader --bench metrics_delta_collect`

**Step 2: Record results in EXPERIMENT.md**

Create or update `EXPERIMENT.md` on the branch with:
- Bound vs unbound counter latency comparison
- Bound vs unbound histogram latency comparison
- Multithread scaling for bound counter
- Non-bound path regression (compare MeasureSteadyState numbers to baseline)

**Step 3: Commit**

```bash
git add EXPERIMENT.md
git commit -m "exp-004: record local benchmark results"
```
