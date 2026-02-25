# Delta Collect Optimization — Fact-Finding Experiment

## Goal

Understand the performance characteristics of OpenTelemetry Rust metrics
collection and measure the impact of data-oriented design changes. This is
an iterative experiment: make changes → run benchmarks → compare results → repeat.

## Benchmark Results Tracking

Each experiment run is tied to a specific git state and stored in S3:

```
s3://oar-benchmarks-20260206210823435100000001/
  runs/
    <timestamp>/<instance-key>/       # raw criterion output
  experiments/
    <experiment-id>.json              # metadata linking commit → results
```

### Experiment ID Format

`EXP-<NNN>-<short-description>`, e.g. `EXP-001-baseline-main`

### Experiment Registry

| ID | Commit | Branch | Description | S3 Timestamp(s) | Status |
|----|--------|--------|-------------|------------------|--------|
| EXP-001 | `5588c32e` | `bench/baseline-for-comparison` | Baseline: main + benchmarks | _first run (bad)_ | Done (no comparison data) |
| EXP-002 | `ecb3eb9b` vs `5588c32e` | `feat/delta-collect-optimization` vs `bench/baseline-for-comparison` | Current feature vs proper baseline | `20260225-1626*` | **Done** |
| EXP-003 | `c8279e9e` vs `5588c32e` | `exp/003-sharded-prehash` vs `bench/baseline-for-comparison` | ShardedMap (16 Mutex shards) + HashedAttributes (foldhash) + PassthroughHasher | `20260225-175*`, `20260225-180*` | **Done — Regression** |

---

## EXP-002 Results

**Branches**: `feat/delta-collect-optimization` (Relaxed ordering, stale eviction, remove trackers_for_collect)
vs `bench/baseline-for-comparison` (main + benchmark files)

**Instances**: c7i.4xlarge (Intel Xeon 8488C), c7a.4xlarge (AMD EPYC 9R14), c7g.4xlarge (Graviton3) — 16 vCPUs, SMT off

**Bold** = statistically significant (p < 0.05). Negative = faster on feature branch.

### Hot Path (measure)

| Benchmark | AMD EPYC 9R14 | Graviton3 | Xeon 8488C |
|-----------|:---:|:---:|:---:|
| MeasureSteadyState/Counter/Cumulative | **-0.75%** | -0.29% | -1.75% |
| MeasureSteadyState/Counter/Delta | **-0.67%** | **+0.87%** | -1.42% |
| MeasureSteadyState/Histogram/Cumulative | **-2.35%** | +0.42% | **-1.50%** |
| MeasureSteadyState/Histogram/Delta | -0.27% | +6.28% | +0.91% |
| MeasureAfterCollect/Counter_1000ts/Cumulative | **-1.31%** | **-1.83%** | -0.78% |
| MeasureAfterCollect/Counter_1000ts/Delta | **-1.24%** | +4.40% | **+0.97%** |
| MeasureAfterCollectIsolated/Counter_1000ts/Cumulative | **-0.47%** | **-0.78%** | -1.26% |
| MeasureAfterCollectIsolated/Counter_1000ts/Delta | +1.00% | **-1.19%** | **-0.59%** |
| MultithreadMeasureAfterCollect/4t/Cumulative | **-4.69%** | **+4.72%** | **-2.50%** |
| MultithreadMeasureAfterCollect/4t/Delta | **+42.04%** | **-2.15%** | **+20.23%** |
| MultithreadMeasureAfterCollect/8t/Cumulative | **-3.21%** | **-7.69%** | **-23.26%** |
| MultithreadMeasureAfterCollect/8t/Delta | **+58.56%** | **-6.66%** | **-13.04%** |
| MultithreadMeasureAfterCollect/12t/Cumulative | **-2.07%** | **-12.56%** | **+19.79%** |
| MultithreadMeasureAfterCollect/12t/Delta | **-0.84%** | **-5.76%** | **+33.46%** |

### Collect Path

| Benchmark | AMD EPYC 9R14 | Graviton3 | Xeon 8488C |
|-----------|:---:|:---:|:---:|
| SequentialCollects/10cycles_1000ts/Cumulative | **-0.31%** | **-1.63%** | **+0.83%** |
| SequentialCollects/10cycles_1000ts/Delta | **-0.40%** | **-1.73%** | -0.92% |

### Mixed (realistic)

| Benchmark | AMD EPYC 9R14 | Graviton3 | Xeon 8488C |
|-----------|:---:|:---:|:---:|
| MixedWorkload/Counter_1000m_1c/Cumulative | **-1.79%** | **+2.48%** | **+2.17%** |
| MixedWorkload/Counter_1000m_1c/Delta | **-1.68%** | **+2.50%** | +0.88% |
| MixedWorkload/Histogram_1000m_1c/Cumulative | **-1.42%** | +0.57% | +0.02% |
| MixedWorkload/Histogram_1000m_1c/Delta | **-1.42%** | **+1.85%** | -2.82% |
| MultithreadWithCollect/4t/Cumulative | **-33.33%** | **+6.49%** | **-4.46%** |
| MultithreadWithCollect/4t/Delta | **-2.30%** | +0.58% | **-33.91%** |
| MultithreadWithCollect/4t_concurrent/Cumulative | **-0.74%** | **+1.97%** | **-13.65%** |
| MultithreadWithCollect/4t_concurrent/Delta | **-2.20%** | **+3.15%** | -0.03% |
| MultithreadMeasureDuringCollect/2t/Delta | **+30.32%** | **-8.47%** | **-6.84%** |
| MultithreadMeasureDuringCollect/4t/Delta | **+50.45%** | **-11.33%** | **-26.02%** |
| MultithreadMeasureDuringCollect/8t/Delta | **+58.91%** | **-9.23%** | **-23.67%** |

### Write Contention

| Benchmark | AMD EPYC 9R14 | Graviton3 | Xeon 8488C |
|-----------|:---:|:---:|:---:|
| WriteContention/2t/Delta | **+4.16%** | **+6.54%** | **+8.62%** |
| WriteContention/4t/Delta | **-8.25%** | **-9.49%** | **+12.86%** |
| WriteContention/8t/Delta | **+2.62%** | **-10.60%** | **-13.78%** |

### EXP-002 Analysis

**What EXP-002 tested**: Small, targeted changes to the existing architecture — Relaxed
atomic ordering on `has_been_updated`, stale entry eviction during delta collect, removing
the `trackers_for_collect` intermediate allocation.

**Observations**:
1. **Single-thread measure**: Marginal improvement (~1-2%) on AMD, noise elsewhere. Expected —
   these changes don't touch the measure hot path (HashMap hashing/comparison still dominates).
2. **Collect path**: Small consistent improvement on AMD/Graviton (~0.4-1.7%), noise on Intel.
   The stale eviction and allocation removal help but the effect is tiny.
3. **Multithread benchmarks**: Results are **wildly inconsistent across architectures**.
   AMD shows +42-59% regressions on some multithread-measure-during-collect scenarios while
   Graviton/Intel show improvements in the same benchmarks. This is likely scheduling/contention
   artifacts from the `Relaxed` ordering change — different CPU memory models react differently.
4. **Bottom line**: These micro-optimizations move the needle by <2% on the collect path.
   The real bottleneck (78% CPU in HashMap hashing + key comparison) is untouched.
   A fundamentally different data structure is needed — which is what EXP-003 targets.

---

## EXP-003 Results

**Branches**: `exp/003-sharded-prehash` (ShardedMap + HashedAttributes + PassthroughHasher)
vs `bench/baseline-for-comparison` (main + benchmark files)

**Design**: Replaced `RwLock<HashMap<Vec<KeyValue>, Arc<TrackerEntry>>>` with 16 `Mutex<HashMap<HashedAttributes, TrackerEntry, PassthroughBuildHasher>>` shards. Attributes are sorted+deduped+hashed once via foldhash into `HashedAttributes`. PassthroughHasher skips re-hashing inside HashMap. No Arc (single entry per attribute set). Overflow handling moved into ShardedMap.

**Instances**: c7i (Intel), c7a (AMD), c7g (Graviton) — large/xlarge/4xlarge

**Result: Uniform regression across all architectures and instance sizes.**

### 4xlarge Results (16 vCPUs, most stable)

| Benchmark | AMD c7a.4xl | Graviton c7g.4xl | Intel c7i.4xl |
|-----------|:---:|:---:|:---:|
| MeasureSteadyState/Counter/Cumulative | **+23.8%** | **+32.5%** | **+10.5%** |
| MeasureSteadyState/Counter/Delta | **+24.0%** | **+32.2%** | **+11.7%** |
| MeasureSteadyState/Histogram/Cumulative | **+21.2%** | **+31.8%** | **+9.6%** |
| MeasureSteadyState/Histogram/Delta | **+22.5%** | **+31.7%** | **+7.9%** |
| MeasureAfterCollect/Counter_1000ts/Cumulative | **+28.3%** | **+33.1%** | **+13.5%** |
| MeasureAfterCollect/Counter_1000ts/Delta | **+31.5%** | **+33.3%** | **+12.0%** |
| SequentialCollects/10cycles_1000ts/Cumulative | **+30.1%** | **+33.0%** | **+10.8%** |
| SequentialCollects/10cycles_1000ts/Delta | **+26.3%** | **+32.5%** | **+14.0%** |
| CollectLatency/* | no change | no change | no change |
| MixedWorkload/* | no change | no change | no change |
| MultithreadWithCollect/* | no change | no change | no change |
| MultithreadMeasureAfterCollect/* | no change | no change | no change |
| CollectOnly/* | no change | no change | no change |
| CollectHeavyWorkload/* | no change | no change | no change |
| MultithreadMeasureDuringCollect/* | no change | no change | no change |
| WriteContention/* | no change | no change | no change |

### Smaller Instance Results (consistent pattern)

| Instance | MeasureSteadyState/Counter regression |
|----------|:---:|
| c7i.large (1 vCPU) | **+11.0%** |
| c7i.xlarge (2 vCPUs) | **+6.3%** |
| c7i.4xlarge (8 vCPUs) | **+10.5%** |
| c7a.large (2 vCPUs) | **+20.9%** |
| c7a.xlarge (4 vCPUs) | **+23.6%** |
| c7a.4xlarge (16 vCPUs) | **+23.8%** |
| c7g.large (2 vCPUs) | **+31.3%** |
| c7g.xlarge (4 vCPUs) | **+26.9%** |
| c7g.4xlarge (16 vCPUs) | **+32.5%** |

### EXP-003 Analysis

**What EXP-003 tested**: Replace the single `RwLock<HashMap>` in ValueMap with 16 Mutex-guarded
shards, pre-hash attributes via foldhash, skip re-hashing via PassthroughHasher, remove Arc
indirection, store one entry per attribute set (always sorted).

**Root cause of regression**: `HashedAttributes::from_raw()` runs on every `measure()` call:
`allocate Vec + sort + dedup + foldhash`. This is **more expensive** than the old approach which
does a direct SipHash lookup with the raw key first (fast-path hit in most cases), and only
falls back to sort+dedup on cache miss. The old code's two-lookup pattern (raw → sorted) is
actually well-optimized for the steady-state case where keys arrive in the same order.

**Why sharding didn't help**: The benchmarks don't generate enough lock contention to benefit
from sharding. The critical section (HashMap get/insert) is extremely short, so the RwLock
read path rarely blocks. Even the WriteContention and MultithreadMeasureDuringCollect benchmarks
showed no significant change — the Mutex overhead per shard roughly equals the RwLock overhead
on the single map.

**Key takeaway**: The measure() hot path is dominated by **allocation and key processing**
(sort+dedup+hash), not lock contention. Sharding addresses contention that isn't the bottleneck.
To actually improve performance, we need to **avoid repeated sort/hash on the measure path**:
- **Bound instruments** (issue #1374): cache the resolved tracker reference, skip lookup entirely
- **Attribute interning**: assign integer IDs on first encounter, O(1) hash thereafter
- **SmallVec**: avoid heap allocation for sort+dedup when attribute count is small

---

## Current Architecture (as of main)

```
ValueMap<A>
  └─ RwLock<HashMap<Vec<KeyValue>, Arc<TrackerEntry<A>>>>
                       │                     │
                  heap-alloc key         heap-alloc, scattered
                  hashed on every        pointer chase per access
                  measure() call         cache miss during collect iteration
```

### Profiling Summary (from flamegraph on local machine)

- **52%** of CPU: HashMap key hashing (`hash_slice`, `sip13` on KeyValue)
- **26%** of CPU: HashMap key comparison (equality check on `Vec<KeyValue>`)
- **<0.05%** of CPU: actual collect() path
- The hot path is `measure()`, not `collect()`

### Reference: Matrix SDK Case Study

[mnt.io article](https://mnt.io/articles/about-memory-pressure-lock-contention-and-data-oriented-design/)
achieved **78x speedup** by:

1. Eliminating repeated lock acquisition (322k lock acquires → cached data)
2. Fitting hot data into CPU cache lines (64 bytes)
3. Separating hot (sort/filter) from cold (full room state) data

---

## EXP-003 Design (Implemented — Regressed)

Replaced single `RwLock<HashMap>` with sharded pre-hashed storage.

### Actual Implementation

```rust
pub(crate) struct ValueMap<A: Aggregator> {
    trackers: ShardedMap<A>,           // 16 Mutex-guarded shards
    overflow_attrs: HashedAttributes,  // pre-hashed overflow sentinel
    // ... count, no_attribute_tracker, config, cardinality_limit
}

struct ShardedMap<A: Aggregator> {
    shards: Box<[Mutex<HashMap<HashedAttributes, TrackerEntry<A>, PassthroughBuildHasher>>; 16]>,
}

struct HashedAttributes {
    attrs: Vec<KeyValue>,  // sorted, deduped
    hash: u64,             // foldhash, computed once
}
```

### What it addressed vs what it didn't

| Bottleneck | Addressed? | Result |
|------------|-----------|--------|
| SipHash on every measure() | Yes → foldhash + PassthroughHasher | Faster hash but sort+dedup+alloc still needed per call |
| Two-lookup pattern (raw→sorted) | Yes → always sorted, single lookup | But removed the fast-path hit for already-sorted keys |
| RwLock contention | Yes → 16 Mutex shards | Contention wasn't actually the bottleneck |
| Arc pointer chasing | Yes → inline TrackerEntry, no Arc | Minor improvement, overwhelmed by regression |
| HashSet dedup in collect | Yes → one entry per attr set | Simplified code but collect wasn't the bottleneck |

---

## Experiment Workflow

### For each experiment:

1. **Branch**: Create `exp/<NNN>-<description>` off `feat/delta-collect-optimization`
2. **Implement**: Make the targeted change
3. **Local smoke test**: `cargo bench --bench metrics_delta_collect -- --test`
4. **Push**: Push branch to `origin/bryantbiggs`
5. **Update Terraform**: Set `feature_branch` to the exp branch
6. **Run**: `terraform apply` → wait for S3 results → `terraform destroy`
7. **Analyze**: Download results, compare against EXP-002 baseline
8. **Record**: Update experiment registry table above

### Comparing results across experiments:

```bash
# Download two experiment runs
aws s3 cp s3://.../<exp1-timestamp>/intel-4xlarge/bench_comparison.txt /tmp/exp1.txt
aws s3 cp s3://.../<exp2-timestamp>/intel-4xlarge/bench_comparison.txt /tmp/exp2.txt

# Or use criterion's --baseline/--save-baseline locally:
cargo bench -- --save-baseline exp-001
# ... make changes ...
cargo bench -- --baseline exp-001
```

---

## Breaking Change Assessment

| Phase | Public API Impact | Semver | Notes |
|-------|-------------------|--------|-------|
| A (Contiguous storage) | None | patch | ValueMap is `pub(crate)` |
| B (Pre-hashed keys) | None | patch | Internal optimization |
| C (Thread-local) | None | patch | Aggregator trait is internal |
| D (Combined) | None | patch | All changes are internal |

The `Aggregator` trait, `ValueMap`, and `TrackerEntry` are all
`pub(crate)` — not part of the public API. All planned changes are
internal implementation details.

---

## Key Metrics to Track

For each experiment, record these from the **4xlarge instances** (most
stable, least noisy-neighbor effects):

### Hot Path (measure)
- `MeasureSteadyState/Counter/{Cumulative,Delta}` — single-thread measure throughput
- `MultithreadMeasureAfterCollect/{4,8,12}threads` — multi-thread measure throughput
- `WriteContention/{2,4,8}threads` — lock contention under write pressure

### Collect Path
- `CollectLatency/Counter_{10,100,1000}ts/{Cumulative,Delta}` — collect time vs cardinality
- `CollectOnly/Counter_{100,1000}ts/{Cumulative,Delta}` — isolated collect cost
- `SequentialCollects/10cycles_1000ts` — repeated collect overhead

### Mixed (realistic)
- `MixedWorkload/Counter_1000measure_1collect/{Cumulative,Delta}` — typical usage pattern
- `MultithreadWithCollect/4threads_counter` — measure+collect contention
- `CollectHeavyWorkload/{1,10,100}measure_1collect` — varying measure:collect ratios

---

## Open Questions

1. **HashMap alternative**: Would a `BTreeMap` or custom hash map with
   better cache behavior (e.g., Swiss table / `hashbrown`) help? Rust's
   std HashMap already uses hashbrown, so probably not — but worth
   measuring with a simpler key type.

2. **Attribute interning**: Could we intern attribute sets (assign a
   unique integer ID on first encounter) and use that as the HashMap key?
   This would make hashing O(1) instead of O(n_attributes).

3. **Lock-free structures**: Could we use `dashmap` or `flurry` (already
   explored in PR #2305) instead of `RwLock<HashMap>`? The previous
   exploration found mixed results — worth re-evaluating with our new
   benchmarks.

4. **Allocation pressure**: The `sort_and_dedup` path allocates a new
   `Vec<KeyValue>` on cache miss. Could a stack-allocated small-vec
   (e.g., `SmallVec<[KeyValue; 8]>`) help for common cases?
