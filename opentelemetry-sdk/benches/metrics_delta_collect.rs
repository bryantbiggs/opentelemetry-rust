//! Benchmark measuring the impact of delta temporality collection on measurement throughput.
//!
//! This benchmark targets the core problem described in issue #2328:
//! During delta collect, the hashmap is cleared entirely, forcing measure() to re-insert
//! entries (requiring write locks + heap allocations). This benchmark measures:
//!
//! 1. **measure() latency in steady state** - after all attribute sets are hydrated
//! 2. **measure() latency immediately after collect** - the "cold path" after delta drain
//! 3. **collect() latency** - time to collect all data points
//! 4. **measure() throughput with periodic collection** - realistic mixed workload
//! 5. **Multi-threaded measure() throughput with periodic collection** - contention effects

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use opentelemetry::{
    metrics::{Counter, Histogram, MeterProvider as _},
    KeyValue,
};
use opentelemetry_sdk::{
    error::OTelSdkResult,
    metrics::{
        data::ResourceMetrics, reader::MetricReader, ManualReader, Pipeline, SdkMeterProvider,
        Temporality,
    },
};
use rand::{
    rngs::{self},
    Rng, SeedableRng,
};
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Weak};
use std::time::Duration;

#[derive(Clone, Debug)]
struct SharedReader(Arc<dyn MetricReader>);

impl MetricReader for SharedReader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
        self.0.register_pipeline(pipeline)
    }
    fn collect(&self, rm: &mut ResourceMetrics) -> OTelSdkResult {
        self.0.collect(rm)
    }
    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }
    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        self.0.shutdown()
    }
    fn temporality(&self, kind: opentelemetry_sdk::metrics::InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

static ATTRIBUTE_VALUES: [&str; 10] = [
    "value1", "value2", "value3", "value4", "value5", "value6", "value7", "value8", "value9",
    "value10",
];

thread_local! {
    static CURRENT_RNG: RefCell<rngs::SmallRng> = RefCell::new(rngs::SmallRng::from_os_rng());
}

fn random_attrs_3() -> [KeyValue; 3] {
    CURRENT_RNG.with(|rng| {
        let mut rng = rng.borrow_mut();
        [
            KeyValue::new("attr1", ATTRIBUTE_VALUES[rng.random_range(0..10)]),
            KeyValue::new("attr2", ATTRIBUTE_VALUES[rng.random_range(0..10)]),
            KeyValue::new("attr3", ATTRIBUTE_VALUES[rng.random_range(0..10)]),
        ]
    })
}

fn setup_counter(temporality: Temporality) -> (SharedReader, Counter<u64>) {
    let rdr = SharedReader(Arc::new(
        ManualReader::builder()
            .with_temporality(temporality)
            .build(),
    ));
    let provider = SdkMeterProvider::builder()
        .with_reader(rdr.clone())
        .build();
    let counter = provider.meter("bench").u64_counter("bench_counter").build();
    (rdr, counter)
}

fn setup_histogram(temporality: Temporality) -> (SharedReader, Histogram<u64>) {
    let rdr = SharedReader(Arc::new(
        ManualReader::builder()
            .with_temporality(temporality)
            .build(),
    ));
    let provider = SdkMeterProvider::builder()
        .with_reader(rdr.clone())
        .build();
    let histogram = provider
        .meter("bench")
        .u64_histogram("bench_histogram")
        .build();
    (rdr, histogram)
}

/// Hydrate: pre-populate all 1000 attribute combinations so the map is warm
fn hydrate_counter(counter: &Counter<u64>) {
    for a in 0..10 {
        for b in 0..10 {
            for c in 0..10 {
                counter.add(
                    1,
                    &[
                        KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                        KeyValue::new("attr2", ATTRIBUTE_VALUES[b]),
                        KeyValue::new("attr3", ATTRIBUTE_VALUES[c]),
                    ],
                );
            }
        }
    }
}

fn hydrate_histogram(histogram: &Histogram<u64>) {
    for a in 0..10 {
        for b in 0..10 {
            for c in 0..10 {
                histogram.record(
                    42,
                    &[
                        KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                        KeyValue::new("attr2", ATTRIBUTE_VALUES[b]),
                        KeyValue::new("attr3", ATTRIBUTE_VALUES[c]),
                    ],
                );
            }
        }
    }
}

// ============================================================================
// BENCHMARK GROUP 1: measure() in steady state (no collection interference)
// ============================================================================
fn bench_measure_steady_state(c: &mut Criterion) {
    let mut group = c.benchmark_group("MeasureSteadyState");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for temporality in [Temporality::Cumulative, Temporality::Delta] {
        let label = match temporality {
            Temporality::Delta => "Delta",
            _ => "Cumulative",
        };

        let (_, counter) = setup_counter(temporality);
        hydrate_counter(&counter);

        group.bench_function(BenchmarkId::new("Counter", label), |b| {
            b.iter_batched(
                random_attrs_3,
                |attrs| counter.add(1, &attrs),
                BatchSize::SmallInput,
            );
        });

        let (_, histogram) = setup_histogram(temporality);
        hydrate_histogram(&histogram);

        group.bench_function(BenchmarkId::new("Histogram", label), |b| {
            b.iter_batched(
                random_attrs_3,
                |attrs| histogram.record(42, &attrs),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 2: measure() immediately after collect (the "cold path")
// ============================================================================
fn bench_measure_after_collect(c: &mut Criterion) {
    let mut group = c.benchmark_group("MeasureAfterCollect");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for temporality in [Temporality::Cumulative, Temporality::Delta] {
        let label = match temporality {
            Temporality::Delta => "Delta",
            _ => "Cumulative",
        };

        let (rdr, counter) = setup_counter(temporality);
        let mut rm = ResourceMetrics::default();
        hydrate_counter(&counter);
        let _ = rdr.collect(&mut rm);

        group.bench_function(BenchmarkId::new("Counter_1000ts", label), |b| {
            b.iter(|| {
                let _ = rdr.collect(&mut rm);
                for a in 0..10 {
                    for b_idx in 0..10 {
                        for c_idx in 0..10 {
                            counter.add(
                                1,
                                &[
                                    KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                                    KeyValue::new("attr2", ATTRIBUTE_VALUES[b_idx]),
                                    KeyValue::new("attr3", ATTRIBUTE_VALUES[c_idx]),
                                ],
                            );
                        }
                    }
                }
            });
        });
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 3: collect() latency with varying cardinality
// ============================================================================
fn bench_collect_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("CollectLatency");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for cardinality in [10, 100, 1000] {
        for temporality in [Temporality::Cumulative, Temporality::Delta] {
            let label = match temporality {
                Temporality::Delta => "Delta",
                _ => "Cumulative",
            };

            let (rdr, counter) = setup_counter(temporality);
            let mut rm = ResourceMetrics::default();

            let vals_per_attr = match cardinality {
                10 => 3,
                100 => 5,
                1000 => 10,
                _ => 10,
            };

            for a in 0..vals_per_attr {
                for b in 0..vals_per_attr {
                    for c_idx in 0..vals_per_attr {
                        counter.add(
                            1,
                            &[
                                KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                                KeyValue::new("attr2", ATTRIBUTE_VALUES[b]),
                                KeyValue::new("attr3", ATTRIBUTE_VALUES[c_idx]),
                            ],
                        );
                    }
                }
            }

            group.throughput(Throughput::Elements(
                (vals_per_attr * vals_per_attr * vals_per_attr) as u64,
            ));

            group.bench_function(
                BenchmarkId::new(format!("Counter_{cardinality}ts"), label),
                |b| {
                    b.iter(|| {
                        if temporality == Temporality::Delta {
                            for a in 0..vals_per_attr {
                                for b_idx in 0..vals_per_attr {
                                    for c_idx in 0..vals_per_attr {
                                        counter.add(
                                            1,
                                            &[
                                                KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                                                KeyValue::new("attr2", ATTRIBUTE_VALUES[b_idx]),
                                                KeyValue::new("attr3", ATTRIBUTE_VALUES[c_idx]),
                                            ],
                                        );
                                    }
                                }
                            }
                        }
                        let _ = rdr.collect(&mut rm);
                    });
                },
            );
        }
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 4: Realistic mixed workload (measure + periodic collect)
// ============================================================================
fn bench_mixed_workload(c: &mut Criterion) {
    let mut group = c.benchmark_group("MixedWorkload");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    for temporality in [Temporality::Cumulative, Temporality::Delta] {
        let label = match temporality {
            Temporality::Delta => "Delta",
            _ => "Cumulative",
        };

        {
            let (rdr, counter) = setup_counter(temporality);
            let mut rm = ResourceMetrics::default();
            hydrate_counter(&counter);

            group.throughput(Throughput::Elements(1000));
            group.bench_function(
                BenchmarkId::new("Counter_1000measure_1collect", label),
                |b| {
                    b.iter(|| {
                        for _ in 0..1000 {
                            let attrs = random_attrs_3();
                            counter.add(1, &attrs);
                        }
                        let _ = rdr.collect(&mut rm);
                    });
                },
            );
        }

        {
            let (rdr, histogram) = setup_histogram(temporality);
            let mut rm = ResourceMetrics::default();
            hydrate_histogram(&histogram);

            group.throughput(Throughput::Elements(1000));
            group.bench_function(
                BenchmarkId::new("Histogram_1000measure_1collect", label),
                |b| {
                    b.iter(|| {
                        for _ in 0..1000 {
                            let attrs = random_attrs_3();
                            histogram.record(42, &attrs);
                        }
                        let _ = rdr.collect(&mut rm);
                    });
                },
            );
        }
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 5: Multi-threaded throughput with periodic collection
// This is THE benchmark that shows the real-world impact of #2328.
// Multiple threads measure() concurrently while a collector thread
// periodically calls collect(). For delta, the collect drains the map,
// causing all measurement threads to hit write-lock contention.
// ============================================================================
fn bench_multithread_with_collect(c: &mut Criterion) {
    let mut group = c.benchmark_group("MultithreadWithCollect");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(20); // Fewer samples since each iteration is expensive

    let num_worker_threads = 4;
    let measures_per_worker = 2500; // 2500 * 4 = 10,000 total measures per iteration

    for temporality in [Temporality::Cumulative, Temporality::Delta] {
        let label = match temporality {
            Temporality::Delta => "Delta",
            _ => "Cumulative",
        };

        // Counter: N threads measure concurrently, main thread collects after all are done
        {
            let (rdr, counter) = setup_counter(temporality);
            hydrate_counter(&counter);
            let mut rm = ResourceMetrics::default();

            group.throughput(Throughput::Elements(
                (num_worker_threads * measures_per_worker) as u64,
            ));
            group.bench_function(
                BenchmarkId::new(
                    format!("{num_worker_threads}threads_counter"),
                    label,
                ),
                |b| {
                    b.iter(|| {
                        // First: collect (clears map for delta)
                        let _ = rdr.collect(&mut rm);

                        // Then: N threads measure concurrently into the (possibly empty) map
                        std::thread::scope(|s| {
                            for _ in 0..num_worker_threads {
                                let counter_ref = &counter;
                                s.spawn(move || {
                                    for _ in 0..measures_per_worker {
                                        let attrs = random_attrs_3();
                                        counter_ref.add(1, &attrs);
                                    }
                                });
                            }
                        });
                    });
                },
            );
        }

        // Counter: continuous collection during measurement
        {
            let (rdr, counter) = setup_counter(temporality);
            hydrate_counter(&counter);

            group.throughput(Throughput::Elements(
                (num_worker_threads * measures_per_worker) as u64,
            ));
            group.bench_function(
                BenchmarkId::new(
                    format!("{num_worker_threads}threads_counter_concurrent_collect"),
                    label,
                ),
                |b| {
                    b.iter(|| {
                        let done = Arc::new(AtomicBool::new(false));
                        let barrier = Arc::new(Barrier::new(num_worker_threads + 1));

                        std::thread::scope(|s| {
                            // Collector thread: collect repeatedly until workers finish
                            let rdr_clone = rdr.clone();
                            let done_clone = done.clone();
                            let barrier_clone = barrier.clone();
                            s.spawn(move || {
                                let mut rm = ResourceMetrics::default();
                                barrier_clone.wait();
                                while !done_clone.load(Ordering::Relaxed) {
                                    let _ = rdr_clone.collect(&mut rm);
                                    std::thread::yield_now();
                                }
                                // Final collect
                                let _ = rdr_clone.collect(&mut rm);
                            });

                            // Worker threads: measure as fast as possible
                            for _ in 0..num_worker_threads {
                                let barrier_clone = barrier.clone();
                                let done_clone = done.clone();
                                let counter_ref = &counter;
                                s.spawn(move || {
                                    barrier_clone.wait();
                                    for _ in 0..measures_per_worker {
                                        let attrs = random_attrs_3();
                                        counter_ref.add(1, &attrs);
                                    }
                                    done_clone.store(true, Ordering::Relaxed);
                                });
                            }
                        });
                    });
                },
            );
        }
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 6: Sequential collects (allocation churn over time)
// ============================================================================
fn bench_sequential_collects(c: &mut Criterion) {
    let mut group = c.benchmark_group("SequentialCollects");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for temporality in [Temporality::Cumulative, Temporality::Delta] {
        let label = match temporality {
            Temporality::Delta => "Delta",
            _ => "Cumulative",
        };

        let (rdr, counter) = setup_counter(temporality);
        let mut rm = ResourceMetrics::default();
        hydrate_counter(&counter);

        group.bench_function(BenchmarkId::new("10cycles_1000ts", label), |b| {
            b.iter(|| {
                for _cycle in 0..10 {
                    for a in 0..10 {
                        for b_idx in 0..10 {
                            for c_idx in 0..10 {
                                counter.add(
                                    1,
                                    &[
                                        KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                                        KeyValue::new("attr2", ATTRIBUTE_VALUES[b_idx]),
                                        KeyValue::new("attr3", ATTRIBUTE_VALUES[c_idx]),
                                    ],
                                );
                            }
                        }
                    }
                    let _ = rdr.collect(&mut rm);
                }
            });
        });
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 7: Isolated post-collect measure() (collect in setup)
// This isolates the core improvement: on main, delta collect drains the map,
// so all subsequent measure() calls hit the write-lock insertion path. On the
// optimized branch, the map is preserved, so measure() stays on the read-lock
// fast path. By moving collect() to setup, we measure ONLY the measure() cost.
// ============================================================================
fn bench_measure_after_collect_isolated(c: &mut Criterion) {
    let mut group = c.benchmark_group("MeasureAfterCollectIsolated");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    for temporality in [Temporality::Cumulative, Temporality::Delta] {
        let label = match temporality {
            Temporality::Delta => "Delta",
            _ => "Cumulative",
        };

        let (rdr, counter) = setup_counter(temporality);
        let mut rm = ResourceMetrics::default();
        hydrate_counter(&counter);

        group.throughput(Throughput::Elements(1000));
        group.bench_function(BenchmarkId::new("Counter_1000ts", label), |b| {
            b.iter_batched(
                || {
                    let _ = rdr.collect(&mut rm);
                },
                |_| {
                    for a in 0..10 {
                        for b_idx in 0..10 {
                            for c_idx in 0..10 {
                                counter.add(
                                    1,
                                    &[
                                        KeyValue::new("attr1", ATTRIBUTE_VALUES[a]),
                                        KeyValue::new("attr2", ATTRIBUTE_VALUES[b_idx]),
                                        KeyValue::new("attr3", ATTRIBUTE_VALUES[c_idx]),
                                    ],
                                );
                            }
                        }
                    }
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

// ============================================================================
// BENCHMARK GROUP 8: Multi-threaded post-collect measure() (collect in setup)
// This is where write-lock contention manifests most clearly. On main, after
// delta collect drains the map, all worker threads compete for the write lock
// to re-insert entries. On the optimized branch, all threads use concurrent
// read locks with no contention.
// ============================================================================
fn bench_multithread_measure_after_collect(c: &mut Criterion) {
    let mut group = c.benchmark_group("MultithreadMeasureAfterCollect");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(20);

    let total_measures = 10_000;

    for num_worker_threads in [4, 8, 12] {
        let measures_per_worker = total_measures / num_worker_threads;

        for temporality in [Temporality::Cumulative, Temporality::Delta] {
            let label = match temporality {
                Temporality::Delta => "Delta",
                _ => "Cumulative",
            };

            let (rdr, counter) = setup_counter(temporality);
            let mut rm = ResourceMetrics::default();
            hydrate_counter(&counter);

            group.throughput(Throughput::Elements(
                (num_worker_threads * measures_per_worker) as u64,
            ));
            group.bench_function(
                BenchmarkId::new(format!("{num_worker_threads}threads_counter"), label),
                |b| {
                    b.iter_batched(
                        || {
                            let _ = rdr.collect(&mut rm);
                        },
                        |_| {
                            std::thread::scope(|s| {
                                for _ in 0..num_worker_threads {
                                    let counter_ref = &counter;
                                    s.spawn(move || {
                                        for _ in 0..measures_per_worker {
                                            let attrs = random_attrs_3();
                                            counter_ref.add(1, &attrs);
                                        }
                                    });
                                }
                            });
                        },
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }
    group.finish();
}

criterion_group! {
    name = delta_collect_benches;
    config = Criterion::default();
    targets =
        bench_measure_steady_state,
        bench_measure_after_collect,
        bench_collect_latency,
        bench_mixed_workload,
        bench_multithread_with_collect,
        bench_sequential_collects,
        bench_measure_after_collect_isolated,
        bench_multithread_measure_after_collect
}

criterion_main!(delta_collect_benches);
