# EXP-004: Bound Instruments — Direct Aggregator Handle

**Date**: 2026-02-25
**Branch**: `exp/004-bound-instruments` (off `feat/delta-collect-optimization`)
**Goal**: Benchmark proof-of-concept for bound instruments that cache the resolved tracker reference, skipping per-call sort+hash+lock+lookup.

## Background

EXP-003 showed that the bottleneck in `ValueMap::measure()` is **per-call allocation and key processing** — not lock contention. Every `counter.add(value, &[kv])` call:

1. Clones the attribute slice to a `Vec<KeyValue>` (~20ns)
2. Sorts it O(n log n) (~30ns)
3. Dedups (~5ns)
4. Hashes via foldhash (~15ns)
5. Locks a shard mutex (~20ns)
6. Does a HashMap lookup (~30ns)
7. Performs an atomic add (~5ns)

Steps 1-6 are redundant when the same attribute set is used repeatedly. Bound instruments eliminate them by caching the resolved tracker reference at bind time.

## Design

### Approach: Direct Aggregator Handle with Poisoning

A user calls `counter.bind(&[kv])` once. This performs steps 1-6 and returns a `BoundCounter<T>` that holds an `Arc<TrackerEntry<A>>`. Subsequent `bound.add(value)` calls go directly to step 7.

A poison flag on `TrackerEntry` detects stale handles after delta collect eviction, falling back to the normal unbound path. No data loss.

### Type Changes

#### TrackerEntry gains `evicted` flag

```rust
struct TrackerEntry<A> {
    aggregator: A,
    has_been_updated: AtomicBool,
    evicted: AtomicBool,  // Set during delta collect eviction
}
```

#### ShardedMap stores Arc-wrapped entries

```rust
// Before:
type ShardHashMap<A> = HashMap<HashedAttributes, TrackerEntry<A>, PassthroughBuildHasher>;

// After:
type ShardHashMap<A> = HashMap<HashedAttributes, Arc<TrackerEntry<A>>, PassthroughBuildHasher>;
```

#### ValueMap gains `bind()` method

```rust
impl<A: Aggregator> ValueMap<A> {
    fn bind(&self, attributes: &[KeyValue]) -> Arc<TrackerEntry<A>> {
        let hashed = HashedAttributes::from_raw(attributes);
        let idx = ShardedMap::<A>::shard_index(hashed.hash);
        let mut shard = self.trackers.shards[idx].lock().unwrap();

        if let Some(entry) = shard.get(&hashed) {
            return Arc::clone(entry);
        }

        // Insert new entry, return Arc clone
        let entry = Arc::new(TrackerEntry::new(A::create(&self.config)));
        shard.insert(hashed, Arc::clone(&entry));
        self.count.fetch_add(1, Ordering::SeqCst);
        entry
    }
}
```

### Measure Trait Extension

```rust
pub(crate) trait Measure<T>: Send + Sync + 'static {
    fn call(&self, measurement: T, attrs: &[KeyValue]);
    fn bind(&self, attrs: &[KeyValue]) -> Box<dyn BoundMeasure<T>>;
}

pub(crate) trait BoundMeasure<T>: Send + Sync + 'static {
    fn call(&self, measurement: T);
}
```

#### Counter bound handle (`Sum<T>`)

```rust
struct BoundSumHandle<T: Number> {
    entry: Arc<TrackerEntry<Increment<T>>>,
    fallback_measure: Arc<dyn Measure<T>>,
    attrs: Vec<KeyValue>,
}

impl<T: Number> BoundMeasure<T> for BoundSumHandle<T> {
    fn call(&self, value: T) {
        if self.entry.evicted.load(Ordering::Acquire) {
            self.fallback_measure.call(value, &self.attrs);
            return;
        }
        self.entry.aggregator.update(value);
        self.entry.has_been_updated.store(true, Ordering::Relaxed);
    }
}
```

#### Histogram bound handle

```rust
struct BoundHistogramHandle<T: Number> {
    entry: Arc<TrackerEntry<Mutex<Buckets<T>>>>,
    bounds: Vec<f64>,
    fallback_measure: Arc<dyn Measure<T>>,
    attrs: Vec<KeyValue>,
}

impl<T: Number> BoundMeasure<T> for BoundHistogramHandle<T> {
    fn call(&self, value: T) {
        if self.entry.evicted.load(Ordering::Acquire) {
            self.fallback_measure.call(value, &self.attrs);
            return;
        }
        let f = value.into_float();
        let index = self.bounds.partition_point(|&x| x < f);
        self.entry.aggregator.update((value, index));
        self.entry.has_been_updated.store(true, Ordering::Relaxed);
    }
}
```

### Public API (PoC)

```rust
// opentelemetry/src/metrics/instruments/counter.rs
pub struct BoundCounter<T> {
    handles: Vec<Box<dyn BoundMeasure<T>>>,
}

impl<T: Copy + 'static> BoundCounter<T> {
    pub fn add(&self, value: T) {
        for handle in &self.handles {
            handle.call(value);
        }
    }
}

impl<T> Counter<T> {
    pub fn bind(&self, attributes: &[KeyValue]) -> BoundCounter<T> { ... }
}

// Similar for Histogram<T> -> BoundHistogram<T>
```

### Poisoning — Delta Collect Eviction

During `collect_and_reset`, set the poison flag before removing stale entries:

```rust
self.trackers.retain(|attrs, entry| {
    if entry.has_been_updated.load(Ordering::Relaxed)
        || *attrs == self.overflow_attrs
    {
        true
    } else {
        entry.evicted.store(true, Ordering::Release);  // Poison before removal
        evicted += 1;
        false
    }
});
```

Bound handles check `evicted` on every `add()`. If poisoned, fall back to the normal unbound path (full sort+hash+lock+lookup). The fallback re-inserts the entry into the HashMap, so no data is lost.

**No self-healing**: after eviction, the stale handle stays in fallback mode permanently. New `bind()` calls get the correct (new) entry. Self-healing is a production optimization, not needed for the PoC benchmark.

### Benchmarks

New benchmark cases in `metrics_delta_collect.rs`:

| Benchmark | Description |
|-----------|-------------|
| `bound_counter_add` | Throughput of `bound.add(1)` — theoretical ceiling |
| `bound_histogram_record` | Same for histogram |
| `bound_vs_unbound_counter` | Side-by-side at same attribute count |
| `bound_counter_add_multithread` | Contention with bound handles across threads |

### Expected Performance

| Path | Estimated Cost | Notes |
|------|---------------|-------|
| Unbound `counter.add()` | ~125-200ns | Same as today |
| Bound `counter.add()` | ~7ns | 1ns poison check + 5ns atomic add + 1ns flag store |
| Bound `histogram.record()` | ~15-20ns | + bucket binary search + per-bucket mutex |
| Arc overhead on unbound path | ~0-4ns | Extra pointer indirection in HashMap |

### Known Limitations (PoC)

1. **No self-healing**: Poisoned bound handles stay in fallback mode. User must call `bind()` again to get a fast handle.
2. **Filter interaction**: Attribute filter is applied at bind time. If the filter could change (it currently cannot), the handle would be stale.
3. **Cumulative temporality**: No eviction occurs, so poisoning never triggers. Bound handles work correctly forever.

### Success Criteria

- Bound `counter.add()` < 15ns (10x improvement over unbound)
- Bound `histogram.record()` < 30ns
- Non-bound path regression < 5%
- No data loss under poisoning (verified by test)

## Prior Art

- **Prometheus client_golang**: "Child" pattern caches resolved metric reference. 30ns uncontended.
- **OTel .NET**: MetricPoint reclaim with `int.MinValue` poisoning. Took 3 PRs over 2 years.
- **OTel Java**: ConcurrentHashMap with cached hashcodes. Epoch rotation for stale handles.
- **OTel Rust issue #1374**: Bound instruments discussion (open).

## Files to Modify

| File | Changes |
|------|---------|
| `opentelemetry-sdk/src/metrics/internal/mod.rs` | `TrackerEntry` (add evicted), `ShardedMap` (Arc values, bind method), `ValueMap` (bind method, collect_and_reset poisoning) |
| `opentelemetry-sdk/src/metrics/internal/sum.rs` | `Sum<T>` implements `Measure::bind()`, `BoundSumHandle` struct |
| `opentelemetry-sdk/src/metrics/internal/histogram.rs` | `Histogram<T>` implements `Measure::bind()`, `BoundHistogramHandle` struct |
| `opentelemetry-sdk/src/metrics/internal/aggregate.rs` | `Measure` trait gains `bind()`, `BoundMeasure` trait definition |
| `opentelemetry-sdk/src/metrics/instrument.rs` | `ResolvedMeasures<T>` implements `bind()`, routes to all pipeline measures |
| `opentelemetry/src/metrics/instruments/counter.rs` | `BoundCounter<T>` struct, `Counter::bind()` |
| `opentelemetry/src/metrics/instruments/histogram.rs` | `BoundHistogram<T>` struct, `Histogram::bind()` |
| `opentelemetry/src/metrics/instruments/mod.rs` | `SyncInstrument` trait gains `bind()` |
| `opentelemetry-sdk/benches/metrics_delta_collect.rs` | New bound instrument benchmarks |
